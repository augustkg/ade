/// A single-line editable text field that tracks a cursor position so the user
/// can move with arrow keys, Home, End, and delete in either direction. Used
/// by all input forms in ADE.
///
/// Cursor is a byte index, kept on a char boundary at all times. Our input is
/// restricted to ASCII so byte index == char index in practice, but the
/// boundary-aware helpers make Unicode-safe behaviour cheap to add later.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TextField {
    value: String,
    cursor: usize,
}

impl TextField {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_str(s: &str) -> Self {
        let value = s.to_string();
        let cursor = value.len();
        Self { value, cursor }
    }

    pub fn as_str(&self) -> &str {
        &self.value
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn is_empty(&self) -> bool {
        self.value.is_empty()
    }

    pub fn trim(&self) -> &str {
        self.value.trim()
    }

    pub fn insert(&mut self, c: char) {
        self.value.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    pub fn delete_left(&mut self) {
        if self.cursor == 0 {
            return;
        }
        if let Some(c) = self.value[..self.cursor].chars().next_back() {
            let len = c.len_utf8();
            self.cursor -= len;
            self.value
                .replace_range(self.cursor..self.cursor + len, "");
        }
    }

    pub fn delete_right(&mut self) {
        if self.cursor >= self.value.len() {
            return;
        }
        if let Some(c) = self.value[self.cursor..].chars().next() {
            let end = self.cursor + c.len_utf8();
            self.value.replace_range(self.cursor..end, "");
        }
    }

    pub fn move_left(&mut self) {
        if self.cursor == 0 {
            return;
        }
        if let Some(c) = self.value[..self.cursor].chars().next_back() {
            self.cursor -= c.len_utf8();
        }
    }

    pub fn move_right(&mut self) {
        if self.cursor >= self.value.len() {
            return;
        }
        if let Some(c) = self.value[self.cursor..].chars().next() {
            self.cursor += c.len_utf8();
        }
    }

    pub fn move_home(&mut self) {
        self.cursor = 0;
    }

    pub fn move_end(&mut self) {
        self.cursor = self.value.len();
    }

    pub fn clear(&mut self) {
        self.value.clear();
        self.cursor = 0;
    }
}
