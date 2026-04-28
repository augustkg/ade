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

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

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
