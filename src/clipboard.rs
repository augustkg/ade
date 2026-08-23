//! Clipboard output for ADE's outer terminal.
//!
//! Both preview selection and embedded tmux sessions converge here. The
//! embedded PTY reader never writes to stdout directly: it queues decoded
//! clipboard text, then the main event loop emits one canonical OSC 52 update
//! between ratatui draws.

use std::io::{self, Write};

use base64::{engine::general_purpose::STANDARD, Engine as _};

/// Keep clipboard traffic bounded even if an application in an embedded pane
/// deliberately emits a huge OSC 52 sequence.
pub const MAX_CLIPBOARD_BYTES: usize = 1024 * 1024;
const MAX_ENCODED_BYTES: usize = MAX_CLIPBOARD_BYTES.div_ceil(3) * 4;

/// Emit text to the outer terminal clipboard using the canonical `c` target.
/// tmux with `set-clipboard on` accepts this from applications and forwards it
/// to its client terminal; direct iTerm sessions consume it themselves.
pub fn emit_text(text: &str) -> io::Result<()> {
    let sequence = osc52_sequence(text)?;
    let mut out = io::stdout().lock();
    out.write_all(&sequence)?;
    out.flush()
}

fn osc52_sequence(text: &str) -> io::Result<Vec<u8>> {
    if text.len() > MAX_CLIPBOARD_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "clipboard text exceeds ADE's 1 MiB limit",
        ));
    }
    let encoded = STANDARD.encode(text.as_bytes());
    let mut sequence = Vec::with_capacity(encoded.len() + 9);
    write!(sequence, "\x1b]52;c;{}\x07", encoded)?;
    Ok(sequence)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScanState {
    Ground,
    Esc,
    Osc,
    OscEsc,
}

/// Streaming recognizer for OSC 52 output. PTY reads may split an escape
/// sequence at any byte, so a per-chunk search is insufficient.
pub struct Osc52Scanner {
    state: ScanState,
    candidate: Vec<u8>,
}

impl Osc52Scanner {
    pub fn new() -> Self {
        Self {
            state: ScanState::Ground,
            candidate: Vec::new(),
        }
    }

    /// Consume terminal output and return every valid clipboard update found.
    /// Invalid or oversized OSC strings are ignored; all original bytes still
    /// go independently to the VT100 parser.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<String> {
        let mut updates = Vec::new();
        for &byte in bytes {
            match self.state {
                ScanState::Ground => {
                    if byte == 0x1b {
                        self.state = ScanState::Esc;
                    }
                }
                ScanState::Esc => {
                    if byte == b']' {
                        self.candidate.clear();
                        self.candidate.extend_from_slice(b"\x1b]");
                        self.state = ScanState::Osc;
                    } else if byte != 0x1b {
                        self.state = ScanState::Ground;
                    }
                }
                ScanState::Osc => match byte {
                    0x07 => {
                        if let Some(text) = parse_osc52(&self.candidate) {
                            updates.push(text);
                        }
                        self.reset();
                    }
                    0x1b => self.state = ScanState::OscEsc,
                    _ => {
                        self.candidate.push(byte);
                        if self.candidate.len() > MAX_ENCODED_BYTES + 32 {
                            self.reset();
                        }
                    }
                },
                ScanState::OscEsc => {
                    if byte == b'\\' {
                        if let Some(text) = parse_osc52(&self.candidate) {
                            updates.push(text);
                        }
                        self.reset();
                    } else {
                        // An ESC inside an OSC payload is invalid. Treat `ESC ]`
                        // as the beginning of a fresh candidate so recovery is
                        // immediate when malformed output precedes a valid OSC.
                        self.candidate.clear();
                        self.state = if byte == b']' {
                            self.candidate.extend_from_slice(b"\x1b]");
                            ScanState::Osc
                        } else if byte == 0x1b {
                            ScanState::Esc
                        } else {
                            ScanState::Ground
                        };
                    }
                }
            }
        }
        updates
    }

    fn reset(&mut self) {
        self.state = ScanState::Ground;
        self.candidate.clear();
    }
}

impl Default for Osc52Scanner {
    fn default() -> Self {
        Self::new()
    }
}

fn parse_osc52(candidate: &[u8]) -> Option<String> {
    let body = candidate.strip_prefix(b"\x1b]52;")?;
    let separator = body.iter().position(|b| *b == b';')?;
    let encoded = &body[separator + 1..];
    if encoded.is_empty() || encoded == b"?" || encoded.len() > MAX_ENCODED_BYTES {
        return None;
    }
    let decoded = STANDARD.decode(encoded).ok()?;
    if decoded.len() > MAX_CLIPBOARD_BYTES {
        return None;
    }
    String::from_utf8(decoded).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_bel_terminated_osc52() {
        let mut scanner = Osc52Scanner::new();
        assert_eq!(
            scanner.feed(b"before\x1b]52;c;aGVsbG8=\x07after"),
            vec!["hello"]
        );
    }

    #[test]
    fn scans_st_terminated_osc52_across_reads() {
        let mut scanner = Osc52Scanner::new();
        assert!(scanner.feed(b"\x1b]52;c;bXVsdG").is_empty());
        assert!(scanner.feed(b"lsaW5lXG50").is_empty());
        assert_eq!(scanner.feed(b"ZXh0\x1b\\"), vec!["multiline\\ntext"]);
    }

    #[test]
    fn ignores_other_osc_and_clipboard_queries() {
        let mut scanner = Osc52Scanner::new();
        assert!(scanner.feed(b"\x1b]0;title\x07\x1b]52;c;?\x07").is_empty());
    }

    #[test]
    fn recovers_after_malformed_base64() {
        let mut scanner = Osc52Scanner::new();
        let updates = scanner.feed(b"\x1b]52;c;!!!\x07\x1b]52;c;b2s=\x07");
        assert_eq!(updates, vec!["ok"]);
    }

    #[test]
    fn returns_multiple_updates_in_order() {
        let mut scanner = Osc52Scanner::new();
        let updates = scanner.feed(b"\x1b]52;c;b25l\x07x\x1b]52;;dHdv\x07");
        assert_eq!(updates, vec!["one", "two"]);
    }

    #[test]
    fn outer_sequence_is_canonical_and_round_trips() {
        let sequence = osc52_sequence("copied: æøå").expect("encode");
        assert!(sequence.starts_with(b"\x1b]52;c;"));
        assert!(sequence.ends_with(b"\x07"));
        let mut scanner = Osc52Scanner::new();
        assert_eq!(scanner.feed(&sequence), vec!["copied: æøå"]);
    }

    #[test]
    fn outer_sequence_rejects_oversized_text() {
        let text = "x".repeat(MAX_CLIPBOARD_BYTES + 1);
        assert_eq!(
            osc52_sequence(&text).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }
}
