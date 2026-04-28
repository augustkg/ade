//! Embedded terminal: spawn `tmux attach -t =name:` (or remote variant)
//! inside a PTY, run a reader thread that pumps output bytes into a
//! `vt100::Parser`, expose write/resize/grid for the UI layer.
//!
//! This module is the heart of the "Tab into the session" feature. The
//! TUI's right pane renders the latest grid state; keystrokes the user
//! types while focused are forwarded byte-by-byte to the PTY; mouse
//! events while hovering the right pane are forwarded as tmux mouse
//! escape sequences.

#![allow(dead_code)] // populated incrementally across the embedded-terminal phases

use std::sync::{Arc, Mutex};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

/// Translate a `crossterm::KeyEvent` into the bytes a terminal would
/// expect to receive when the user pressed that key. Used to forward
/// keystrokes from ADE's outer event loop into the embedded PTY while
/// the right panel is focused.
///
/// Coverage:
/// - Printable chars (UTF-8).
/// - Ctrl-letter (a–z, A–Z) → control codes 0x01..0x1a.
/// - Ctrl with `@`, `[`, `\`, `]`, `^`, `_` → 0x00, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f.
/// - Alt-X → ESC prefix + the bytes for X (xterm's "meta as ESC" mode).
/// - Tab (HT), BackTab (CSI Z), Enter (CR), Backspace (DEL = 0x7f), Esc.
/// - Arrows + Home / End / PageUp / PageDown / Insert / Delete with
///   optional modifiers, encoded as `CSI 1 ; <mod> <letter>` per xterm.
/// - Function keys F1–F12.
///
/// Returns an empty vec for keys we don't recognise (e.g. Caps Lock,
/// media keys) so the caller can ignore unhandled events without panic.
pub fn translate_key(event: KeyEvent) -> Vec<u8> {
    let m = event.modifiers;
    match event.code {
        KeyCode::Char(c) => translate_char(c, m),
        KeyCode::Enter => prefix_alt(m, b"\r".to_vec()),
        KeyCode::Tab => prefix_alt(m, b"\t".to_vec()),
        KeyCode::BackTab => prefix_alt(m, b"\x1b[Z".to_vec()),
        KeyCode::Backspace => prefix_alt(m, b"\x7f".to_vec()),
        KeyCode::Esc => prefix_alt(m, b"\x1b".to_vec()),
        KeyCode::Up => csi_arrow_or_modified(m, b'A'),
        KeyCode::Down => csi_arrow_or_modified(m, b'B'),
        KeyCode::Right => csi_arrow_or_modified(m, b'C'),
        KeyCode::Left => csi_arrow_or_modified(m, b'D'),
        KeyCode::Home => csi_arrow_or_modified(m, b'H'),
        KeyCode::End => csi_arrow_or_modified(m, b'F'),
        KeyCode::PageUp => csi_tilde_or_modified(m, 5),
        KeyCode::PageDown => csi_tilde_or_modified(m, 6),
        KeyCode::Insert => csi_tilde_or_modified(m, 2),
        KeyCode::Delete => csi_tilde_or_modified(m, 3),
        KeyCode::F(n) => function_key(n, m),
        _ => Vec::new(),
    }
}

fn translate_char(c: char, m: KeyModifiers) -> Vec<u8> {
    // Ctrl-letter: A–Z, a–z. Mask off the high bits so 'a'/'A' → 0x01.
    if m.contains(KeyModifiers::CONTROL) {
        // Ctrl + special char fallbacks.
        if let Some(b) = ctrl_special(c) {
            return prefix_alt_extra(m, vec![b]);
        }
        if c.is_ascii_alphabetic() {
            let b = (c.to_ascii_uppercase() as u8) & 0x1f;
            return prefix_alt_extra(m, vec![b]);
        }
        // Other Ctrl combos (e.g. Ctrl+1) — fall through to plain char.
    }

    let mut bytes = c.to_string().into_bytes();
    if m.contains(KeyModifiers::ALT) {
        let mut out = Vec::with_capacity(bytes.len() + 1);
        out.push(0x1b);
        out.append(&mut bytes);
        return out;
    }
    bytes
}

fn ctrl_special(c: char) -> Option<u8> {
    // Two flavours of representation are mapped here:
    //
    // (a) The "logical" form (`'@'`, `'['`, `'\\'`, `']'`, `'^'`, `'_'`,
    //     `'?'`, `' '`) — what an application would synthesize if it
    //     constructed a `KeyEvent { code: Char('\\'), modifiers: CTRL }`.
    //
    // (b) The "raw" form (`'4'`, `'5'`, `'6'`, `'7'`) — what crossterm's
    //     own legacy-mode parser emits for incoming control bytes
    //     `0x1c..0x1f`. See the parser in
    //     `crossterm-0.28.1/src/event/sys/unix/parse.rs` (the
    //     `c @ b'\x1C'..=b'\x1F'` arm reports `Char((c - 0x1c) + b'4')
    //     + CONTROL`). Without (b), real keyboard `Ctrl+\` never
    //     produces 0x1c and the embedded-mode exit chord can't fire.
    match c {
        '@' | ' ' => Some(0x00), // Ctrl+@ and Ctrl+space both → NUL
        '[' => Some(0x1b),
        '\\' | '4' => Some(0x1c),
        ']' | '5' => Some(0x1d),
        '^' | '6' => Some(0x1e),
        '_' | '?' | '7' => Some(0x1f),
        _ => None,
    }
}

fn prefix_alt(m: KeyModifiers, mut bytes: Vec<u8>) -> Vec<u8> {
    if m.contains(KeyModifiers::ALT) {
        let mut out = Vec::with_capacity(bytes.len() + 1);
        out.push(0x1b);
        out.append(&mut bytes);
        out
    } else {
        bytes
    }
}

fn prefix_alt_extra(m: KeyModifiers, bytes: Vec<u8>) -> Vec<u8> {
    // For Ctrl-combos that also have Alt: prepend ESC to the control byte.
    prefix_alt(m, bytes)
}

/// xterm modifier code per the standard:
///   shift = 1, alt = 2, ctrl = 4 → bitmask + 1.
/// No modifier → 1, which most parsers treat as "no modifier".
fn xterm_modifier(m: KeyModifiers) -> u8 {
    let mut bits: u8 = 0;
    if m.contains(KeyModifiers::SHIFT) {
        bits |= 1;
    }
    if m.contains(KeyModifiers::ALT) {
        bits |= 2;
    }
    if m.contains(KeyModifiers::CONTROL) {
        bits |= 4;
    }
    1 + bits
}

fn csi_arrow_or_modified(m: KeyModifiers, letter: u8) -> Vec<u8> {
    let mod_code = xterm_modifier(m);
    if mod_code == 1 {
        vec![0x1b, b'[', letter]
    } else {
        // CSI 1 ; <mod> <letter>
        format!("\x1b[1;{}{}", mod_code, letter as char).into_bytes()
    }
}

fn csi_tilde_or_modified(m: KeyModifiers, n: u8) -> Vec<u8> {
    let mod_code = xterm_modifier(m);
    if mod_code == 1 {
        format!("\x1b[{}~", n).into_bytes()
    } else {
        format!("\x1b[{};{}~", n, mod_code).into_bytes()
    }
}

/// State machine for the embedded-mode exit chord.
///
/// We need a way to leave embedded mode without conflicting with keys
/// the embedded session itself uses. Bare Esc breaks vim; bare Tab is
/// the entry key. The classic approach is a tmux-style prefix chord:
///
///   `Ctrl+\` then `q`  →  exit embedded mode
///   `Ctrl+\` then any other key  →  forward buffered prefix + key
///   `Ctrl+\` then `Ctrl+\`  →  forward exactly one literal prefix
///                              (escape hatch for sessions that use
///                              Ctrl+\ for their own purposes)
///
/// `Ctrl+\` is a sane choice because almost no shell or TUI binds it
/// (bash treats it as SIGQUIT *only* on terminals where INTR/QUIT keys
/// are routed by termios; inside tmux it's typically free).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChordState {
    Idle,
    Pending,
}

impl ChordState {
    pub fn new() -> Self {
        ChordState::Idle
    }
}

impl Default for ChordState {
    fn default() -> Self {
        Self::new()
    }
}

/// Outcome of feeding one `KeyEvent` into the chord state machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChordOutcome {
    /// Forward these bytes to the PTY (could be empty if a state
    /// transition consumed the event without producing data).
    Forward(Vec<u8>),
    /// Exit embedded mode. The caller should detach the PTY and
    /// return focus to the tree.
    Exit,
}

impl ChordOutcome {
    pub fn forward_bytes(b: Vec<u8>) -> Self {
        ChordOutcome::Forward(b)
    }
}

/// The exit-chord prefix as a byte. Anything that translates to this
/// byte (whether the "logical" `Char('\\') + CONTROL` form or the raw
/// `Char('4') + CONTROL` form crossterm emits) is treated as the chord
/// prefix.
pub const CHORD_PREFIX_BYTE: u8 = 0x1c;

/// Drive the chord state machine with one event. Returns the outcome.
/// The caller is responsible for actually writing the bytes to the PTY
/// or exiting embedded mode.
///
/// Only `KeyEventKind::Press` events are honoured. Repeat/Release events
/// (which crossterm's enhanced keyboard protocols can synthesise) are
/// no-ops here: a Release for `Ctrl+\` while `Pending` would otherwise
/// erroneously fire the prefix-passthrough path. The TUI's main event
/// loop already filters to Press today, but enforcing the contract at
/// the function boundary keeps it correct under future protocol changes.
pub fn chord_step(state: &mut ChordState, event: KeyEvent) -> ChordOutcome {
    use crossterm::event::KeyEventKind;
    if event.kind != KeyEventKind::Press {
        return ChordOutcome::Forward(Vec::new());
    }
    let translated = translate_key(event);

    match *state {
        ChordState::Idle => {
            if translated == [CHORD_PREFIX_BYTE] {
                *state = ChordState::Pending;
                ChordOutcome::Forward(Vec::new())
            } else {
                ChordOutcome::Forward(translated)
            }
        }
        ChordState::Pending => {
            // Anything that emerges from Pending puts us back in Idle.
            *state = ChordState::Idle;
            // `q` is the exit verb. Plain 'q' / 'Q' with no modifiers
            // (or just SHIFT) — Ctrl+q etc. should pass through.
            if matches!(event.code, KeyCode::Char('q' | 'Q'))
                && !event.modifiers.contains(KeyModifiers::CONTROL)
                && !event.modifiers.contains(KeyModifiers::ALT)
            {
                return ChordOutcome::Exit;
            }
            // Chord-prefix passthrough: Ctrl+\ Ctrl+\ → send exactly one
            // literal Ctrl+\ byte (the buffered one), discarding the
            // second so we don't double-emit.
            if translated == [CHORD_PREFIX_BYTE] {
                return ChordOutcome::Forward(vec![CHORD_PREFIX_BYTE]);
            }
            // Generic case: emit the buffered prefix THEN the key bytes.
            // The session sees the chord prefix it would have got, plus
            // whatever the user actually typed next.
            let mut out = vec![CHORD_PREFIX_BYTE];
            out.extend(translated);
            ChordOutcome::Forward(out)
        }
    }
}

fn function_key(n: u8, m: KeyModifiers) -> Vec<u8> {
    let mod_code = xterm_modifier(m);
    // F1–F4 use SS3 ("\x1bO<letter>"); F5–F12 use CSI <code>~.
    let unmodified: Vec<u8> = match n {
        1 => b"\x1bOP".to_vec(),
        2 => b"\x1bOQ".to_vec(),
        3 => b"\x1bOR".to_vec(),
        4 => b"\x1bOS".to_vec(),
        5 => b"\x1b[15~".to_vec(),
        6 => b"\x1b[17~".to_vec(),
        7 => b"\x1b[18~".to_vec(),
        8 => b"\x1b[19~".to_vec(),
        9 => b"\x1b[20~".to_vec(),
        10 => b"\x1b[21~".to_vec(),
        11 => b"\x1b[23~".to_vec(),
        12 => b"\x1b[24~".to_vec(),
        _ => return Vec::new(),
    };
    if mod_code == 1 {
        return unmodified;
    }
    // For modified F-keys, xterm uses the "1;<mod>" form for F1–F4 and a
    // "<code>;<mod>~" form for F5+. Translate accordingly.
    match n {
        1 => format!("\x1b[1;{}P", mod_code).into_bytes(),
        2 => format!("\x1b[1;{}Q", mod_code).into_bytes(),
        3 => format!("\x1b[1;{}R", mod_code).into_bytes(),
        4 => format!("\x1b[1;{}S", mod_code).into_bytes(),
        5 => format!("\x1b[15;{}~", mod_code).into_bytes(),
        6 => format!("\x1b[17;{}~", mod_code).into_bytes(),
        7 => format!("\x1b[18;{}~", mod_code).into_bytes(),
        8 => format!("\x1b[19;{}~", mod_code).into_bytes(),
        9 => format!("\x1b[20;{}~", mod_code).into_bytes(),
        10 => format!("\x1b[21;{}~", mod_code).into_bytes(),
        11 => format!("\x1b[23;{}~", mod_code).into_bytes(),
        12 => format!("\x1b[24;{}~", mod_code).into_bytes(),
        _ => Vec::new(),
    }
}

/// Shared `vt100::Parser` — written to by the reader thread and read
/// from by the renderer (UI thread). Wrapped so we can hand a clone to
/// the worker without surrendering exclusive access on the UI side.
pub type SharedParser = Arc<Mutex<vt100::Parser>>;

/// Hand-render the current `vt100::Screen` into a list of `ratatui::Line`s
/// preserving foreground/background colour and style attributes. We
/// don't go through `tui-term` because its widget targets a different
/// `Widget` trait than ratatui 0.29 exposes through `Frame::render_widget`
/// (the same kind of version-pin mismatch we hit with `ansi-to-tui` v8).
pub fn screen_to_lines<'a>(screen: &vt100::Screen) -> Vec<Line<'a>> {
    let (rows, cols) = screen.size();
    let mut out: Vec<Line> = Vec::with_capacity(rows as usize);
    for row in 0..rows {
        let mut spans: Vec<Span> = Vec::new();
        let mut cur_text = String::new();
        let mut cur_style: Option<Style> = None;

        for col in 0..cols {
            let Some(cell) = screen.cell(row, col) else {
                continue;
            };
            // Skip the second half of a wide character — its glyph already
            // appeared in the previous cell. Without this, the literal " "
            // we emit for empty contents would shift everything right.
            if cell.is_wide_continuation() {
                continue;
            }
            let text = cell.contents();
            let glyph = if text.is_empty() {
                " ".to_string()
            } else {
                text.to_string()
            };
            let style = cell_style(cell);
            match cur_style {
                Some(s) if s == style => cur_text.push_str(&glyph),
                _ => {
                    if let Some(s) = cur_style.take() {
                        spans.push(Span::styled(std::mem::take(&mut cur_text), s));
                    }
                    cur_text.push_str(&glyph);
                    cur_style = Some(style);
                }
            }
        }
        if let Some(s) = cur_style.take() {
            spans.push(Span::styled(cur_text, s));
        }
        out.push(Line::from(spans));
    }
    out
}

fn cell_style(cell: &vt100::Cell) -> Style {
    let mut style = Style::default();
    if let Some(fg) = vt_to_ratatui_color(cell.fgcolor()) {
        style = style.fg(fg);
    }
    if let Some(bg) = vt_to_ratatui_color(cell.bgcolor()) {
        style = style.bg(bg);
    }
    let mut modifier = Modifier::empty();
    if cell.bold() {
        modifier |= Modifier::BOLD;
    }
    if cell.italic() {
        modifier |= Modifier::ITALIC;
    }
    if cell.underline() {
        modifier |= Modifier::UNDERLINED;
    }
    if cell.inverse() {
        modifier |= Modifier::REVERSED;
    }
    style.add_modifier(modifier)
}

fn vt_to_ratatui_color(c: vt100::Color) -> Option<Color> {
    match c {
        vt100::Color::Default => None,
        vt100::Color::Idx(0) => Some(Color::Black),
        vt100::Color::Idx(1) => Some(Color::Red),
        vt100::Color::Idx(2) => Some(Color::Green),
        vt100::Color::Idx(3) => Some(Color::Yellow),
        vt100::Color::Idx(4) => Some(Color::Blue),
        vt100::Color::Idx(5) => Some(Color::Magenta),
        vt100::Color::Idx(6) => Some(Color::Cyan),
        vt100::Color::Idx(7) => Some(Color::Gray),
        vt100::Color::Idx(8) => Some(Color::DarkGray),
        vt100::Color::Idx(9) => Some(Color::LightRed),
        vt100::Color::Idx(10) => Some(Color::LightGreen),
        vt100::Color::Idx(11) => Some(Color::LightYellow),
        vt100::Color::Idx(12) => Some(Color::LightBlue),
        vt100::Color::Idx(13) => Some(Color::LightMagenta),
        vt100::Color::Idx(14) => Some(Color::LightCyan),
        vt100::Color::Idx(15) => Some(Color::White),
        vt100::Color::Idx(n) => Some(Color::Indexed(n)),
        vt100::Color::Rgb(r, g, b) => Some(Color::Rgb(r, g, b)),
    }
}

#[cfg(test)]
mod key_translate_tests {
    use super::translate_key;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

    fn ev(code: KeyCode, m: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: m,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    #[test]
    fn plain_char_a() {
        assert_eq!(translate_key(ev(KeyCode::Char('a'), KeyModifiers::NONE)), b"a");
    }

    #[test]
    fn shift_char_uppercase() {
        // crossterm gives us the shifted char already, so 'A' goes through as-is.
        assert_eq!(
            translate_key(ev(KeyCode::Char('A'), KeyModifiers::SHIFT)),
            b"A"
        );
    }

    #[test]
    fn ctrl_letter_a_to_soh() {
        assert_eq!(
            translate_key(ev(KeyCode::Char('a'), KeyModifiers::CONTROL)),
            &[0x01]
        );
    }

    #[test]
    fn ctrl_letter_c_to_etx() {
        assert_eq!(
            translate_key(ev(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            &[0x03]
        );
    }

    #[test]
    fn ctrl_letter_works_for_uppercase_too() {
        // Some terminals report Ctrl+C as ('C', SHIFT|CONTROL); we treat
        // alphabetic case identically.
        assert_eq!(
            translate_key(ev(
                KeyCode::Char('C'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            )),
            &[0x03]
        );
    }

    #[test]
    fn ctrl_backslash_to_fs_logical_form() {
        // The exit-chord prefix in its "logical" form, i.e. when a
        // KeyEvent is constructed with `Char('\\')` and CONTROL.
        assert_eq!(
            translate_key(ev(KeyCode::Char('\\'), KeyModifiers::CONTROL)),
            &[0x1c]
        );
    }

    #[test]
    fn ctrl_backslash_to_fs_raw_form() {
        // What crossterm's parser actually produces for the user
        // physically pressing Ctrl+\ on the keyboard. This is the
        // codepath the exit chord depends on at runtime; without this
        // mapping, real Ctrl+\ would emit literal "4".
        assert_eq!(
            translate_key(ev(KeyCode::Char('4'), KeyModifiers::CONTROL)),
            &[0x1c]
        );
    }

    #[test]
    fn ctrl_left_bracket_to_esc() {
        assert_eq!(
            translate_key(ev(KeyCode::Char('['), KeyModifiers::CONTROL)),
            &[0x1b]
        );
    }

    #[test]
    fn ctrl_raw_forms_for_1c_through_1f() {
        // Codex review: crossterm 0.28 reports incoming bytes 0x1c..0x1f
        // as Char('4')..Char('7') + CONTROL. Cover all four to guard
        // against regression.
        assert_eq!(
            translate_key(ev(KeyCode::Char('4'), KeyModifiers::CONTROL)),
            &[0x1c]
        );
        assert_eq!(
            translate_key(ev(KeyCode::Char('5'), KeyModifiers::CONTROL)),
            &[0x1d]
        );
        assert_eq!(
            translate_key(ev(KeyCode::Char('6'), KeyModifiers::CONTROL)),
            &[0x1e]
        );
        assert_eq!(
            translate_key(ev(KeyCode::Char('7'), KeyModifiers::CONTROL)),
            &[0x1f]
        );
    }

    #[test]
    fn alt_a_prepends_esc() {
        assert_eq!(
            translate_key(ev(KeyCode::Char('a'), KeyModifiers::ALT)),
            &[0x1b, b'a']
        );
    }

    #[test]
    fn tab_is_ht() {
        assert_eq!(translate_key(ev(KeyCode::Tab, KeyModifiers::NONE)), b"\t");
    }

    #[test]
    fn backtab_is_csi_z() {
        assert_eq!(
            translate_key(ev(KeyCode::BackTab, KeyModifiers::NONE)),
            b"\x1b[Z"
        );
    }

    #[test]
    fn enter_is_cr() {
        assert_eq!(translate_key(ev(KeyCode::Enter, KeyModifiers::NONE)), b"\r");
    }

    #[test]
    fn backspace_is_del() {
        assert_eq!(
            translate_key(ev(KeyCode::Backspace, KeyModifiers::NONE)),
            &[0x7f]
        );
    }

    #[test]
    fn esc_is_esc() {
        assert_eq!(translate_key(ev(KeyCode::Esc, KeyModifiers::NONE)), &[0x1b]);
    }

    #[test]
    fn arrow_up_unmodified() {
        assert_eq!(translate_key(ev(KeyCode::Up, KeyModifiers::NONE)), b"\x1b[A");
    }

    #[test]
    fn arrow_right_with_ctrl() {
        // Ctrl = 4 bits → modifier code 5 → CSI 1;5C
        assert_eq!(
            translate_key(ev(KeyCode::Right, KeyModifiers::CONTROL)),
            b"\x1b[1;5C"
        );
    }

    #[test]
    fn arrow_left_with_shift() {
        // Shift = 1 bit → modifier code 2 → CSI 1;2D
        assert_eq!(
            translate_key(ev(KeyCode::Left, KeyModifiers::SHIFT)),
            b"\x1b[1;2D"
        );
    }

    #[test]
    fn page_up_unmodified() {
        assert_eq!(
            translate_key(ev(KeyCode::PageUp, KeyModifiers::NONE)),
            b"\x1b[5~"
        );
    }

    #[test]
    fn page_down_with_ctrl() {
        assert_eq!(
            translate_key(ev(KeyCode::PageDown, KeyModifiers::CONTROL)),
            b"\x1b[6;5~"
        );
    }

    #[test]
    fn delete_unmodified() {
        assert_eq!(
            translate_key(ev(KeyCode::Delete, KeyModifiers::NONE)),
            b"\x1b[3~"
        );
    }

    #[test]
    fn f1_is_ss3_p() {
        assert_eq!(translate_key(ev(KeyCode::F(1), KeyModifiers::NONE)), b"\x1bOP");
    }

    #[test]
    fn f5_is_csi_15_tilde() {
        assert_eq!(
            translate_key(ev(KeyCode::F(5), KeyModifiers::NONE)),
            b"\x1b[15~"
        );
    }

    #[test]
    fn f12_is_csi_24_tilde() {
        assert_eq!(
            translate_key(ev(KeyCode::F(12), KeyModifiers::NONE)),
            b"\x1b[24~"
        );
    }

    #[test]
    fn unknown_key_returns_empty() {
        // Capslock/Pause/etc. We don't crash; we just send nothing.
        assert!(translate_key(ev(KeyCode::CapsLock, KeyModifiers::NONE)).is_empty());
    }

    #[test]
    fn unicode_char_is_utf8_encoded() {
        assert_eq!(
            translate_key(ev(KeyCode::Char('é'), KeyModifiers::NONE)),
            "é".as_bytes()
        );
    }
}

#[cfg(test)]
mod chord_tests {
    use super::{chord_step, ChordOutcome, ChordState, CHORD_PREFIX_BYTE};
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

    fn ev(code: KeyCode, m: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: m,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    #[test]
    fn idle_plain_key_forwards_translation() {
        let mut s = ChordState::Idle;
        let out = chord_step(&mut s, ev(KeyCode::Char('a'), KeyModifiers::NONE));
        assert_eq!(out, ChordOutcome::Forward(b"a".to_vec()));
        assert_eq!(s, ChordState::Idle);
    }

    #[test]
    fn idle_ctrl_backslash_logical_enters_pending() {
        let mut s = ChordState::Idle;
        let out = chord_step(
            &mut s,
            ev(KeyCode::Char('\\'), KeyModifiers::CONTROL),
        );
        assert_eq!(out, ChordOutcome::Forward(Vec::new()));
        assert_eq!(s, ChordState::Pending);
    }

    #[test]
    fn idle_ctrl_backslash_raw_form_enters_pending() {
        // The crossterm-actually-emits form. This is what fires at runtime.
        let mut s = ChordState::Idle;
        let out = chord_step(
            &mut s,
            ev(KeyCode::Char('4'), KeyModifiers::CONTROL),
        );
        assert_eq!(out, ChordOutcome::Forward(Vec::new()));
        assert_eq!(s, ChordState::Pending);
    }

    #[test]
    fn pending_q_exits() {
        let mut s = ChordState::Pending;
        let out = chord_step(&mut s, ev(KeyCode::Char('q'), KeyModifiers::NONE));
        assert_eq!(out, ChordOutcome::Exit);
        assert_eq!(s, ChordState::Idle);
    }

    #[test]
    fn pending_capital_q_also_exits() {
        // SHIFT+q is fine — it's still a "q" with the user's intent.
        let mut s = ChordState::Pending;
        let out = chord_step(
            &mut s,
            ev(KeyCode::Char('Q'), KeyModifiers::SHIFT),
        );
        assert_eq!(out, ChordOutcome::Exit);
    }

    #[test]
    fn pending_ctrl_q_does_not_exit_passes_through() {
        // Ctrl+q in shells = XON/restart-output. Don't hijack it.
        let mut s = ChordState::Pending;
        let out = chord_step(
            &mut s,
            ev(KeyCode::Char('q'), KeyModifiers::CONTROL),
        );
        // Buffered prefix + Ctrl+q (0x11)
        assert_eq!(out, ChordOutcome::Forward(vec![CHORD_PREFIX_BYTE, 0x11]));
        assert_eq!(s, ChordState::Idle);
    }

    #[test]
    fn pending_ctrl_backslash_passes_one_literal_prefix() {
        // Escape hatch: chord-prefix-prefix sends exactly one literal
        // prefix byte to the embedded session, not two.
        let mut s = ChordState::Pending;
        let out = chord_step(
            &mut s,
            ev(KeyCode::Char('4'), KeyModifiers::CONTROL),
        );
        assert_eq!(
            out,
            ChordOutcome::Forward(vec![CHORD_PREFIX_BYTE])
        );
        assert_eq!(s, ChordState::Idle);
    }

    #[test]
    fn pending_other_key_forwards_buffered_plus_new() {
        // E.g. user pressed Ctrl+\ then 'a' (changed mind, didn't want
        // to exit). Session should see the prefix it expected plus 'a'.
        let mut s = ChordState::Pending;
        let out = chord_step(&mut s, ev(KeyCode::Char('a'), KeyModifiers::NONE));
        assert_eq!(
            out,
            ChordOutcome::Forward(vec![CHORD_PREFIX_BYTE, b'a'])
        );
        assert_eq!(s, ChordState::Idle);
    }

    #[test]
    fn idle_esc_passes_through_does_not_exit() {
        // Critical: bare Esc must not exit — vim needs Esc.
        let mut s = ChordState::Idle;
        let out = chord_step(&mut s, ev(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(out, ChordOutcome::Forward(vec![0x1b]));
        assert_eq!(s, ChordState::Idle);
    }

    #[test]
    fn full_chord_idle_to_pending_to_exit() {
        let mut s = ChordState::Idle;
        // Ctrl+\
        let _ = chord_step(
            &mut s,
            ev(KeyCode::Char('4'), KeyModifiers::CONTROL),
        );
        assert_eq!(s, ChordState::Pending);
        // q
        let out = chord_step(&mut s, ev(KeyCode::Char('q'), KeyModifiers::NONE));
        assert_eq!(out, ChordOutcome::Exit);
        assert_eq!(s, ChordState::Idle);
    }

    #[test]
    fn release_event_while_pending_is_a_noop() {
        // Codex Phase-3 review: a Release event for Ctrl+\ while in
        // Pending must NOT be treated as a chord-prefix passthrough,
        // or holding-and-releasing the prefix key would emit a literal
        // 0x1c instead of leaving the chord armed.
        let mut s = ChordState::Pending;
        let release = KeyEvent {
            code: KeyCode::Char('4'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Release,
            state: KeyEventState::NONE,
        };
        let out = chord_step(&mut s, release);
        assert_eq!(out, ChordOutcome::Forward(Vec::new()));
        assert_eq!(s, ChordState::Pending, "state must not transition on release");
    }
}

#[cfg(test)]
mod compat_probe {
    //! Phase 1 acceptance: prove that portable-pty + vt100 + our
    //! hand-rendered Spans + ratatui 0.29 compose correctly. If any
    //! step blows up at integration time, we see it here, not at
    //! Phase 9.

    use super::{screen_to_lines, SharedParser};
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};
    use ratatui::{backend::TestBackend, widgets::Paragraph, Terminal};
    use std::io::Read;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    fn make_parser() -> SharedParser {
        Arc::new(Mutex::new(vt100::Parser::new(24, 80, 0)))
    }

    /// Wide-char regression: feed a CJK wide glyph followed by ASCII to
    /// the parser; assert the rendered Lines don't drift right by the
    /// continuation-cell offset (Codex flagged this on Phase 1 review).
    #[test]
    fn wide_char_does_not_shift_following_cells() {
        let mut parser = vt100::Parser::new(24, 80, 0);
        // U+754C ("界") is 2 cells wide. Then 'X' should appear in
        // visual column 2 (cells 2,3 are 'X' and the rest spaces).
        parser.process("界X".as_bytes());
        let lines = screen_to_lines(parser.screen());
        let row0_text: String = lines[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        // First two visual cells = "界", then "X", then spaces. Our
        // renderer skips the wide continuation cell, so the string
        // should be exactly "界X" + 78 spaces.
        assert!(
            row0_text.starts_with("界X"),
            "expected '界X...' at row 0, got: {:?}",
            row0_text
        );
    }

    /// End-to-end probe: PTY → vt100 → screen_to_lines → Paragraph →
    /// ratatui TestBackend → assert "hi" landed in the rendered cells.
    #[cfg(unix)]
    #[test]
    fn pty_to_vt100_to_ratatui_render_pipeline() {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");

        let mut cmd = CommandBuilder::new("/bin/sh");
        cmd.arg("-c");
        cmd.arg("printf hi");
        let mut child = pair.slave.spawn_command(cmd).expect("spawn");

        // Drop the slave so the child owns its end. Otherwise we hold
        // an extra fd open and never see EOF.
        drop(pair.slave);

        let parser = make_parser();
        let mut reader = pair.master.try_clone_reader().expect("clone reader");
        let parser_for_reader = parser.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if let Ok(mut p) = parser_for_reader.lock() {
                            p.process(&buf[..n]);
                        }
                    }
                }
            }
        });

        // Wait until the parser sees "hi" or we time out.
        // 5s is generous — local runs see "hi" in <50ms; the budget
        // protects against slow CI / loaded test machines.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if Instant::now() >= deadline {
                let _ = child.kill();
                panic!("timed out waiting for parser to see 'hi'");
            }
            {
                let p = parser.lock().unwrap();
                let row0 = p.screen().contents().lines().next().unwrap_or("").to_string();
                if row0.starts_with("hi") {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        // Render the parser's screen via screen_to_lines into a ratatui
        // TestBackend and assert the resulting buffer has 'h' at (0,0)
        // and 'i' at (1,0).
        let backend = TestBackend::new(80, 24);
        let mut term = Terminal::new(backend).expect("terminal");
        term.draw(|frame| {
            let p = parser.lock().unwrap();
            let lines = screen_to_lines(p.screen());
            let para = Paragraph::new(lines);
            frame.render_widget(para, frame.area());
        })
        .expect("draw");

        let buf = term.backend().buffer();
        let h = buf[(0, 0)].symbol();
        let i = buf[(1, 0)].symbol();
        assert_eq!(h, "h", "cell (0,0) should be 'h', got {:?}", h);
        assert_eq!(i, "i", "cell (1,0) should be 'i', got {:?}", i);

        let _ = child.wait();
    }
}
