//! A small multi-line text input with a character-indexed cursor.
//!
//! The cursor is tracked as a character offset (not a byte offset) so editing
//! is correct for multi-byte UTF-8, and the on-screen cursor column is computed
//! with [`unicode_width`] so wide CJK glyphs — which the mandated
//! Simplified-Chinese UI will contain — advance the caret by two cells, not one.

use unicode_width::UnicodeWidthChar;

/// An editable text buffer with a character cursor and newline support.
#[derive(Debug, Default, Clone)]
pub struct Input {
    value: String,
    /// Cursor position as a count of characters from the start.
    cursor: usize,
}

impl Input {
    /// Creates an empty input.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The current buffer contents.
    #[must_use]
    pub fn value(&self) -> &str {
        &self.value
    }

    /// `true` when the buffer holds only whitespace (or nothing).
    #[must_use]
    pub fn is_blank(&self) -> bool {
        self.value.trim().is_empty()
    }

    /// Number of characters in the buffer.
    #[must_use]
    pub fn char_len(&self) -> usize {
        self.value.chars().count()
    }

    /// Byte offset of the character cursor within `value`.
    fn cursor_byte(&self) -> usize {
        self.value
            .char_indices()
            .nth(self.cursor)
            .map_or(self.value.len(), |(b, _)| b)
    }

    /// Inserts `c` at the cursor and advances past it.
    pub fn insert_char(&mut self, c: char) {
        let at = self.cursor_byte();
        self.value.insert(at, c);
        self.cursor += 1;
    }

    /// Inserts a hard newline at the cursor.
    pub fn insert_newline(&mut self) {
        self.insert_char('\n');
    }

    /// Inserts a whole string at the cursor (used for bracketed paste).
    pub fn insert_str(&mut self, s: &str) {
        let at = self.cursor_byte();
        self.value.insert_str(at, s);
        self.cursor += s.chars().count();
    }

    /// Deletes the character before the cursor, if any.
    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.cursor -= 1;
        let at = self.cursor_byte();
        self.value.remove(at);
    }

    /// Deletes the character at the cursor, if any.
    pub fn delete(&mut self) {
        if self.cursor >= self.char_len() {
            return;
        }
        let at = self.cursor_byte();
        self.value.remove(at);
    }

    /// Moves the cursor one character left.
    pub fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    /// Moves the cursor one character right.
    pub fn move_right(&mut self) {
        if self.cursor < self.char_len() {
            self.cursor += 1;
        }
    }

    /// Moves the cursor to the start of the buffer.
    pub fn move_home(&mut self) {
        self.cursor = 0;
    }

    /// Moves the cursor to the end of the buffer.
    pub fn move_end(&mut self) {
        self.cursor = self.char_len();
    }

    /// Deletes the word before the cursor (whitespace-delimited).
    pub fn delete_word(&mut self) {
        let mut chars: Vec<char> = self.value.chars().collect();
        let mut i = self.cursor;
        // Skip trailing spaces, then the word.
        while i > 0 && chars[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !chars[i - 1].is_whitespace() {
            i -= 1;
        }
        chars.drain(i..self.cursor);
        self.value = chars.into_iter().collect();
        self.cursor = i;
    }

    /// Clears the buffer and returns its previous contents, trimmed.
    pub fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.value).trim().to_owned()
    }

    /// Empties the buffer without returning the contents.
    pub fn clear(&mut self) {
        self.value.clear();
        self.cursor = 0;
    }

    /// The cursor's display position as `(column, row)` in cells.
    ///
    /// The row counts hard newlines before the cursor; the column is the total
    /// display width (CJK-aware) of the characters since the last newline.
    #[must_use]
    pub fn cursor_col_row(&self) -> (u16, u16) {
        let mut col: usize = 0;
        let mut row: usize = 0;
        for c in self.value.chars().take(self.cursor) {
            if c == '\n' {
                row += 1;
                col = 0;
            } else {
                col += UnicodeWidthChar::width(c).unwrap_or(0);
            }
        }
        (
            u16::try_from(col).unwrap_or(u16::MAX),
            u16::try_from(row).unwrap_or(u16::MAX),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_backspace_ascii() {
        let mut input = Input::new();
        for c in "hello".chars() {
            input.insert_char(c);
        }
        assert_eq!(input.value(), "hello");
        input.backspace();
        assert_eq!(input.value(), "hell");
    }

    #[test]
    fn cursor_is_char_indexed_for_multibyte() {
        let mut input = Input::new();
        for c in "你好".chars() {
            input.insert_char(c);
        }
        assert_eq!(input.char_len(), 2);
        input.move_left();
        input.insert_char('x');
        // x lands between the two CJK characters.
        assert_eq!(input.value(), "你x好");
    }

    #[test]
    fn cjk_cursor_column_counts_two_cells() {
        let mut input = Input::new();
        input.insert_char('你');
        // After one wide char the cursor sits two cells in.
        assert_eq!(input.cursor_col_row(), (2, 0));
    }

    #[test]
    fn newline_advances_row() {
        let mut input = Input::new();
        input.insert_char('a');
        input.insert_newline();
        input.insert_char('b');
        assert_eq!(input.cursor_col_row(), (1, 1));
    }

    #[test]
    fn take_trims_and_clears() {
        let mut input = Input::new();
        input.insert_str("  hi  ");
        assert_eq!(input.take(), "hi");
        assert!(input.is_blank());
    }

    #[test]
    fn delete_word_removes_last_token() {
        let mut input = Input::new();
        input.insert_str("foo bar");
        input.delete_word();
        assert_eq!(input.value(), "foo ");
    }
}

// Rust guideline compliant 2026-02-21
