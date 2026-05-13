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

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use crate::hosts::Host;

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

/// Translate a `crossterm::MouseEvent` whose coords are in *frame*-local
/// space (where (0,0) is the top-left of the entire ADE TUI) into the
/// SGR-1006 mouse escape sequence the embedded terminal expects, with
/// coords adjusted into *pane*-local space (1-based).
///
/// Format: `ESC [ < <code> ; <col> ; <row> M|m`
///   - `M` for press / drag / motion
///   - `m` for release
///   - Button codes:
///       0 left, 1 middle, 2 right
///       +4 shift, +8 alt, +16 ctrl
///       +32 motion (drag with button, or no-button move)
///       64/65/66/67 wheel up/down/left/right
///
/// Returns an empty Vec if the event is outside `pane_rect` (so the
/// caller can ignore mouse traffic that wandered onto the tree side).
pub fn translate_mouse(
    event: MouseEvent,
    pane_rect: (u16, u16, u16, u16),
) -> Vec<u8> {
    let (px, py, pw, ph) = pane_rect;
    if event.column < px
        || event.row < py
        || event.column >= px.saturating_add(pw)
        || event.row >= py.saturating_add(ph)
    {
        return Vec::new();
    }
    // Pane-local, 1-based.
    let col = event.column - px + 1;
    let row = event.row - py + 1;

    let (mut code, action) = match event.kind {
        MouseEventKind::Down(b) => (mouse_button_code(b), 'M'),
        MouseEventKind::Up(b) => (mouse_button_code(b), 'm'),
        MouseEventKind::Drag(b) => (mouse_button_code(b) + 32, 'M'),
        MouseEventKind::Moved => (35, 'M'), // 32 + 3 (no-button motion)
        MouseEventKind::ScrollUp => (64, 'M'),
        MouseEventKind::ScrollDown => (65, 'M'),
        MouseEventKind::ScrollLeft => (66, 'M'),
        MouseEventKind::ScrollRight => (67, 'M'),
    };
    if event.modifiers.contains(KeyModifiers::SHIFT) {
        code |= 4;
    }
    if event.modifiers.contains(KeyModifiers::ALT) {
        code |= 8;
    }
    if event.modifiers.contains(KeyModifiers::CONTROL) {
        code |= 16;
    }
    format!("\x1b[<{};{};{}{}", code, col, row, action).into_bytes()
}

fn mouse_button_code(b: MouseButton) -> u32 {
    match b {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    }
}

/// State machine for the embedded-mode exit chord.
///
/// We need a way to leave embedded mode without conflicting with keys
/// the embedded session itself uses. Bare Esc breaks vim; bare Tab is
/// the entry key. The classic approach is a tmux-style prefix chord:
///
///   `Ctrl+Space` then `Space`        →  exit embedded mode
///   `Ctrl+Space` then any other key  →  forward buffered prefix + key
///   `Ctrl+Space` then `Ctrl+Space`   →  forward exactly one literal
///                                        prefix (escape hatch for
///                                        sessions that use NUL for
///                                        their own purposes)
///
/// `Ctrl+Space` is a deliberate choice over `Ctrl+\` (the previous
/// default). The backslash key on Danish, German, Norwegian, French
/// and many other layouts sits behind AltGr, which makes `Ctrl+\`
/// effectively unreachable for non-US users. `Ctrl+Space` lives in
/// the same place on every keyboard and has minimal collision with
/// shells / TUIs (bash readline binds it to `set-mark` in emacs mode;
/// vim autocomplete plugins occasionally use it). Crossterm reports
/// the keystroke as `Char(' ') + CONTROL` and the translator at
/// `ctrl_special(' ')` already maps it to byte `0x00`.
///
/// **Caveat:** `Ctrl+Space` is sometimes intercepted *before* it
/// reaches ADE — macOS uses it as the default input-source switcher,
/// and CJK IMEs on Linux/Windows often bind it to IME toggle. If a
/// user reports the chord doesn't fire, the first thing to check is
/// whether their OS / input method is grabbing the key (System
/// Settings → Keyboard → Shortcuts → Input Sources on macOS).
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

/// The exit-chord prefix as a byte. Currently `Ctrl+Space` (NUL).
/// Anything that translates to this byte is treated as the chord
/// prefix — that's `Char(' ') + CONTROL` and `Char('@') + CONTROL`,
/// both routed through `ctrl_special` to `0x00`.
pub const CHORD_PREFIX_BYTE: u8 = 0x00;

/// Drive the chord state machine with one event. Returns the outcome.
/// The caller is responsible for actually writing the bytes to the PTY
/// or exiting embedded mode.
///
/// Only `KeyEventKind::Press` events are honoured. Repeat/Release events
/// (which crossterm's enhanced keyboard protocols can synthesise) are
/// no-ops here: a Release for `Ctrl+Space` while `Pending` would
/// otherwise erroneously fire the prefix-passthrough path. The TUI's
/// main event loop already filters to Press today, but enforcing the
/// contract at the function boundary keeps it correct under future
/// protocol changes.
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
            // Plain Space (no Ctrl, no Alt) is the exit verb. Ctrl+Space
            // is the prefix-passthrough escape hatch handled just below.
            if matches!(event.code, KeyCode::Char(' '))
                && !event.modifiers.contains(KeyModifiers::CONTROL)
                && !event.modifiers.contains(KeyModifiers::ALT)
            {
                return ChordOutcome::Exit;
            }
            // Chord-prefix passthrough: Ctrl+Space Ctrl+Space → send
            // exactly one literal NUL byte (the buffered one),
            // discarding the second so we don't double-emit.
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

/// RAII guard for terminal mouse capture. Constructed when entering
/// embedded mode; on Drop it disables mouse capture so the user gets
/// their normal terminal scroll/selection back.
///
/// Tied to `EmbeddedTerm`'s lifetime — enabling capture only while
/// embedded fixes two issues Codex flagged in Phase 7 review:
///  - global capture made tree/preview-mode scroll feel broken
///  - panicking while embedded would have left the terminal in
///    capture mode (Drop runs during unwind)
struct MouseCaptureGuard;

impl MouseCaptureGuard {
    fn enable() -> Self {
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::event::EnableMouseCapture
        );
        Self
    }
}

impl Drop for MouseCaptureGuard {
    fn drop(&mut self) {
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::event::DisableMouseCapture
        );
    }
}

/// One embedded terminal: a PTY pair, the child process attached to it
/// (typically `tmux attach-session -t =name:`), a writer for sending
/// keystrokes, a `vt100::Parser` fed by a reader thread, and the join
/// handle so we can shut down cleanly on `Drop`.
pub struct EmbeddedTerm {
    /// Master side of the PTY. Kept so we can call `resize()`. The
    /// `writer` field is a separate handle to the same fd.
    master: Box<dyn MasterPty + Send>,
    /// Write half — bytes pushed here are read by the child.
    writer: Box<dyn Write + Send>,
    /// Shared parser. The reader thread writes; the UI renderer reads.
    parser: SharedParser,
    /// Handle to the spawned process (`tmux attach`, `ssh`, `mosh`, …).
    child: Box<dyn portable_pty::Child + Send + Sync>,
    /// Reader-thread join handle. `None` after Drop has joined it.
    reader_thread: Option<JoinHandle<()>>,
    /// Last size we resized the PTY to. The renderer calls `resize()`
    /// every frame; this lets us short-circuit when the size hasn't
    /// changed (otherwise vt100::Parser::set_size walks all rows on
    /// every same-size call). Wrapped so resize() can stay `&self`.
    last_size: Mutex<(u16, u16)>,
    /// Mouse-capture is enabled for the duration of an embedded
    /// session and disabled (via Drop) when the session ends —
    /// including on panic. Field declared *before* `reader_thread`
    /// in declaration order is unimportant; what matters is that
    /// it drops with the rest of the struct.
    _mouse_capture: MouseCaptureGuard,
}

impl EmbeddedTerm {
    /// Spawn an embedded `tmux attach-session -t =name` against the
    /// local tmux server. Sized to `(rows, cols)` to match the panel
    /// area the UI will render into.
    pub fn spawn_local(name: &str, rows: u16, cols: u16) -> Result<Self, String> {
        let mut cmd = CommandBuilder::new("tmux");
        cmd.args(["attach-session", "-t", &format!("={}", name)]);
        Self::spawn_with_command(cmd, rows, cols)
    }

    /// Spawn an embedded attach to a remote session via the same
    /// SSH/Mosh routing as `crate::build_attach_command`. We pass
    /// `plant_parent: false` because embedded preview is *not* a parent-
    /// process attach — ADE is still drawing in the host pane, and the
    /// `@ade-parent` marker would mislead the `prefix B` keybinding
    /// (`detach-client` instead of `switch-client -l`) for any future
    /// direct attach to the same session.
    pub fn spawn_remote(
        host: &Host,
        name: &str,
        rows: u16,
        cols: u16,
    ) -> Result<Self, String> {
        let target = format!("={}", name);
        let (program, args) = crate::build_attach_command(host, &target, false);
        let mut cmd = CommandBuilder::new(&program);
        for a in &args {
            cmd.arg(a);
        }
        Self::spawn_with_command(cmd, rows, cols)
    }

    fn spawn_with_command(
        cmd: CommandBuilder,
        rows: u16,
        cols: u16,
    ) -> Result<Self, String> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| format!("openpty: {}", e))?;

        // Take the writer + reader handles BEFORE spawning the child,
        // so a failure here doesn't leak an unreaped child. Codex
        // Phase-4 review caught this: portable-pty's child is just a
        // wrapped std::process::Child, and dropping it doesn't reap.
        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| format!("clone PTY reader: {}", e))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| format!("take PTY writer: {}", e))?;

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| format!("spawn embedded child: {}", e))?;
        // Drop the slave so we don't keep an extra fd open — otherwise
        // the master never sees EOF when the child exits.
        drop(pair.slave);

        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 0)));

        let parser_for_thread = parser.clone();
        let reader_thread = std::thread::spawn(move || {
            let mut buf = [0u8; 8 * 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break, // EOF
                    Ok(n) => {
                        if let Ok(mut p) = parser_for_thread.lock() {
                            p.process(&buf[..n]);
                        }
                    }
                    Err(e) => {
                        // Treat any read error as terminal — close out
                        // cleanly. EBADF / EIO often means the master
                        // closed underneath us during shutdown.
                        let _ = e;
                        break;
                    }
                }
            }
        });

        Ok(Self {
            master: pair.master,
            writer,
            parser,
            child,
            reader_thread: Some(reader_thread),
            last_size: Mutex::new((rows, cols)),
            _mouse_capture: MouseCaptureGuard::enable(),
        })
    }

    /// Forward bytes to the embedded child. No-op for empty input.
    pub fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        self.writer.write_all(bytes)?;
        self.writer.flush()
    }

    /// Resize the PTY and the parser's grid. Call when the panel area
    /// changes (terminal resize, layout flip, etc.).
    ///
    /// Same-size calls short-circuit: the renderer calls this every
    /// frame and `vt100::Parser::set_size` would otherwise walk every
    /// row on each call (Codex Phase-6 review).
    pub fn resize(&self, rows: u16, cols: u16) -> Result<(), String> {
        if let Ok(mut last) = self.last_size.lock() {
            if *last == (rows, cols) {
                return Ok(());
            }
            *last = (rows, cols);
        }
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| format!("PTY resize: {}", e))?;
        if let Ok(mut p) = self.parser.lock() {
            p.set_size(rows, cols);
        }
        Ok(())
    }

    /// A clone of the shared parser. Renderer takes the lock briefly,
    /// reads `screen()`, drops the lock.
    pub fn parser(&self) -> SharedParser {
        self.parser.clone()
    }

    /// `true` if the child is still running. False once it has exited
    /// (e.g. tmux detached, target session was killed externally).
    pub fn is_alive(&mut self) -> bool {
        match self.child.try_wait() {
            Ok(None) => true,         // still running
            Ok(Some(_)) => false,     // exited
            Err(_) => false,
        }
    }

    /// Best-effort kill of the child. Used by the App when the user
    /// fires the exit chord — we want the embedded `tmux attach` to
    /// detach (which kills its process), leaving the underlying tmux
    /// session intact.
    pub fn kill(&mut self) {
        let _ = self.child.kill();
    }
}

impl Drop for EmbeddedTerm {
    fn drop(&mut self) {
        // Drop runs THIS body, then drops fields in declaration order:
        // master, writer, parser, child, reader_thread. The master
        // dropping closes its fd, which causes the reader thread's
        // cloned reader to EOF (or EIO mapped to Ok(0) by portable-pty),
        // so the reader exits naturally and the JoinHandle drop
        // detaches it cleanly.
        //
        // We do NOT call `handle.join()` here. Rust's join is unbounded;
        // if `tmux attach`'s child accidentally spawned a grandchild
        // that inherits the slave fd, the master read would never EOF
        // and join would hang ADE shutdown forever. Detaching is the
        // lesser evil — the OS reaps any straggler when ADE exits.
        let _ = self.child.kill();
        let _ = self.child.wait();
        // Drop the take()'d handle without joining.
        let _ = self.reader_thread.take();
    }
}

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
mod mouse_tests {
    use super::translate_mouse;
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

    fn pane() -> (u16, u16, u16, u16) {
        // Right panel at x=40, y=3, 60 wide, 24 tall.
        (40, 3, 60, 24)
    }

    fn me(kind: MouseEventKind, col: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn click_inside_pane_translates_with_local_coords() {
        // Left-click at frame (40, 3) → pane-local (1, 1) (1-based).
        let bytes =
            translate_mouse(me(MouseEventKind::Down(MouseButton::Left), 40, 3), pane());
        assert_eq!(bytes, b"\x1b[<0;1;1M");
    }

    #[test]
    fn click_outside_pane_returns_empty() {
        // Frame coord (10, 10) is on the tree side; we don't forward.
        let bytes =
            translate_mouse(me(MouseEventKind::Down(MouseButton::Left), 10, 10), pane());
        assert!(bytes.is_empty());
    }

    #[test]
    fn release_uses_lowercase_m() {
        let bytes =
            translate_mouse(me(MouseEventKind::Up(MouseButton::Left), 50, 5), pane());
        assert_eq!(bytes, b"\x1b[<0;11;3m");
    }

    #[test]
    fn drag_adds_motion_bit() {
        // Drag(left) = 0 + 32 = 32, suffix M.
        let bytes =
            translate_mouse(me(MouseEventKind::Drag(MouseButton::Left), 45, 5), pane());
        assert_eq!(bytes, b"\x1b[<32;6;3M");
    }

    #[test]
    fn scroll_up_is_code_64() {
        let bytes = translate_mouse(me(MouseEventKind::ScrollUp, 70, 10), pane());
        assert_eq!(bytes, b"\x1b[<64;31;8M");
    }

    #[test]
    fn scroll_down_is_code_65() {
        let bytes = translate_mouse(me(MouseEventKind::ScrollDown, 70, 10), pane());
        assert_eq!(bytes, b"\x1b[<65;31;8M");
    }

    #[test]
    fn shift_modifier_adds_4() {
        let mut e = me(MouseEventKind::Down(MouseButton::Left), 41, 4);
        e.modifiers = KeyModifiers::SHIFT;
        let bytes = translate_mouse(e, pane());
        // Code 0 + 4 (shift) = 4
        assert_eq!(bytes, b"\x1b[<4;2;2M");
    }

    #[test]
    fn ctrl_scroll_for_zoom_apps() {
        let mut e = me(MouseEventKind::ScrollUp, 50, 8);
        e.modifiers = KeyModifiers::CONTROL;
        let bytes = translate_mouse(e, pane());
        // 64 + 16 = 80
        assert_eq!(bytes, b"\x1b[<80;11;6M");
    }

    #[test]
    fn boundary_right_edge_is_outside() {
        // Pane spans columns 40..=99 (40 + 60 = 100 exclusive).
        // Column 99 is in. Column 100 is not.
        let inside =
            translate_mouse(me(MouseEventKind::Down(MouseButton::Left), 99, 3), pane());
        assert!(!inside.is_empty());
        let outside =
            translate_mouse(me(MouseEventKind::Down(MouseButton::Left), 100, 3), pane());
        assert!(outside.is_empty());
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
    fn idle_ctrl_space_enters_pending() {
        // The chord prefix is `Ctrl+Space` (Danish-keyboard friendly,
        // replacing the older `Ctrl+\` default). crossterm reports
        // this as `Char(' ') + CONTROL`; the translator at
        // `ctrl_special(' ')` maps it to `0x00`, our prefix byte.
        let mut s = ChordState::Idle;
        let out = chord_step(
            &mut s,
            ev(KeyCode::Char(' '), KeyModifiers::CONTROL),
        );
        assert_eq!(out, ChordOutcome::Forward(Vec::new()));
        assert_eq!(s, ChordState::Pending);
    }

    #[test]
    fn idle_ctrl_at_also_enters_pending() {
        // `Ctrl+@` is the alt name for the same byte (0x00) — some
        // keyboards / shells synthesise it instead of `Ctrl+Space`.
        // Both paths must arm the chord.
        let mut s = ChordState::Idle;
        let out = chord_step(
            &mut s,
            ev(KeyCode::Char('@'), KeyModifiers::CONTROL),
        );
        assert_eq!(out, ChordOutcome::Forward(Vec::new()));
        assert_eq!(s, ChordState::Pending);
    }

    #[test]
    fn pending_space_exits() {
        let mut s = ChordState::Pending;
        let out = chord_step(&mut s, ev(KeyCode::Char(' '), KeyModifiers::NONE));
        assert_eq!(out, ChordOutcome::Exit);
        assert_eq!(s, ChordState::Idle);
    }

    #[test]
    fn pending_ctrl_space_passes_one_literal_prefix() {
        // Escape hatch: chord-prefix-prefix sends exactly one literal
        // prefix byte (NUL, 0x00) to the embedded session, not two.
        let mut s = ChordState::Pending;
        let out = chord_step(
            &mut s,
            ev(KeyCode::Char(' '), KeyModifiers::CONTROL),
        );
        assert_eq!(
            out,
            ChordOutcome::Forward(vec![CHORD_PREFIX_BYTE])
        );
        assert_eq!(s, ChordState::Idle);
    }

    #[test]
    fn pending_other_key_forwards_buffered_plus_new() {
        // E.g. user pressed Ctrl+Space then 'a' (changed mind, didn't
        // want to exit). Session should see the prefix it expected
        // (the buffered NUL) plus 'a'.
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
        // Ctrl+Space
        let _ = chord_step(
            &mut s,
            ev(KeyCode::Char(' '), KeyModifiers::CONTROL),
        );
        assert_eq!(s, ChordState::Pending);
        // Space
        let out = chord_step(&mut s, ev(KeyCode::Char(' '), KeyModifiers::NONE));
        assert_eq!(out, ChordOutcome::Exit);
        assert_eq!(s, ChordState::Idle);
    }

    #[test]
    fn release_event_while_pending_is_a_noop() {
        // Codex Phase-3 review: a Release event for Ctrl+Space while
        // in Pending must NOT be treated as a chord-prefix passthrough,
        // or holding-and-releasing the prefix key would emit a literal
        // NUL instead of leaving the chord armed.
        let mut s = ChordState::Pending;
        let release = KeyEvent {
            code: KeyCode::Char(' '),
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
mod embedded_term_tests {
    //! Phase 4: lifecycle smoke test for the EmbeddedTerm wrapper using
    //! a plain `bash` child. Real `tmux attach` lifecycle is exercised
    //! in the Phase 9 acceptance test against an isolated tmux server.

    use super::EmbeddedTerm;
    use portable_pty::CommandBuilder;
    use std::time::{Duration, Instant};

    #[cfg(unix)]
    #[test]
    fn write_read_resize_lifecycle() {
        // Use `bash --norc --noprofile` so we don't pick up the user's
        // dotfiles. Sleep at the end so the child stays alive long
        // enough to assert on resize before EOF.
        let mut cmd = CommandBuilder::new("/bin/bash");
        cmd.args(["--norc", "--noprofile", "-i"]);
        let mut term = EmbeddedTerm::spawn_with_command(cmd, 24, 80).expect("spawn");

        // 1. Write — should land in the parser within a small budget.
        term.write(b"echo lifecycle-ok\n")
            .expect("write to PTY");

        let parser = term.parser();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if Instant::now() >= deadline {
                term.kill();
                panic!("parser never saw 'lifecycle-ok'");
            }
            {
                let p = parser.lock().unwrap();
                let contents = p.screen().contents();
                if contents.contains("lifecycle-ok") {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        // 2. Resize — succeeds without panic and updates the parser's grid.
        term.resize(40, 120).expect("resize PTY");
        {
            let p = parser.lock().unwrap();
            assert_eq!(p.screen().size(), (40, 120));
        }

        // 3. is_alive: still true while child is running.
        assert!(term.is_alive(), "bash should still be running");

        // 4. Drop kills the child + reaps the reader thread.
        drop(term);
        // No assertion here — if Drop hangs, the test deadline above
        // catches us. If it returns, we're good.
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
