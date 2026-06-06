//! A small multi-line text input with a character-indexed cursor.
//!
//! The cursor is tracked as a character offset (not a byte offset) so editing
//! is correct for multi-byte UTF-8, and the on-screen cursor column is computed
//! with [`unicode_width`] so wide CJK glyphs — which the mandated
//! Simplified-Chinese UI will contain — advance the caret by two cells, not one.

use unicode_width::UnicodeWidthChar;

/// Maximum number of history entries retained.
///
/// Keeps memory bounded; oldest entries are evicted once the cap is reached.
const HISTORY_CAP: usize = 100;

/// An editable text buffer with a character cursor, newline support, and
/// a bounded history stack of previously submitted messages.
#[derive(Debug, Default, Clone)]
pub struct Input {
    value: String,
    /// Cursor position as a count of characters from the start.
    cursor: usize,
    /// Submitted messages, oldest first, newest last.
    history: Vec<String>,
    /// `Some(i)` while browsing history; `None` when editing the live draft.
    history_pos: Option<usize>,
    /// Saved draft so that navigating away and back restores unsaved work.
    draft: String,
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
        // Any edit while browsing history drops us back to live mode.
        self.history_pos = None;
    }

    /// Inserts a hard newline at the cursor.
    pub fn insert_newline(&mut self) {
        self.insert_char('\n');
    }

    /// The buffer text from the start up to (but not including) the cursor.
    ///
    /// Used to detect a live `@`-mention token, whose query is the run of
    /// non-whitespace characters between the last `@` and the cursor.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_tui::input::Input;
    /// let mut input = Input::new();
    /// input.insert_str("hello");
    /// input.move_left();
    /// assert_eq!(input.before_cursor(), "hell");
    /// ```
    #[must_use]
    pub fn before_cursor(&self) -> &str {
        &self.value[..self.cursor_byte()]
    }

    /// Replaces the `token_len` chars before the cursor with `@<path> `.
    ///
    /// Removes the `@`-mention token (its `@` plus `token_len - 1` query chars)
    /// ending at the cursor and inserts the resolved `path` wrapped as
    /// `@<path> `, leaving the cursor just past the trailing space so the mention
    /// popup closes. A `token_len` larger than the available prefix is clamped.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_tui::input::Input;
    /// let mut input = Input::new();
    /// input.insert_str("see @ma");
    /// input.replace_mention(3, "src/main.rs");
    /// assert_eq!(input.value(), "see @src/main.rs ");
    /// ```
    pub fn replace_mention(&mut self, token_len: usize, path: &str) {
        let end_char = self.cursor;
        let start_char = end_char.saturating_sub(token_len);
        let start_byte = self
            .value
            .char_indices()
            .nth(start_char)
            .map_or(self.value.len(), |(b, _)| b);
        let end_byte = self
            .value
            .char_indices()
            .nth(end_char)
            .map_or(self.value.len(), |(b, _)| b);
        let insert = format!("@{path} ");
        let insert_chars = insert.chars().count();
        self.value.replace_range(start_byte..end_byte, &insert);
        self.cursor = start_char + insert_chars;
        self.history_pos = None;
    }

    /// Inserts a whole string at the cursor (used for bracketed paste).
    pub fn insert_str(&mut self, s: &str) {
        let at = self.cursor_byte();
        self.value.insert_str(at, s);
        self.cursor += s.chars().count();
        self.history_pos = None;
    }

    /// Deletes the character before the cursor, if any.
    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.cursor -= 1;
        let at = self.cursor_byte();
        self.value.remove(at);
        self.history_pos = None;
    }

    /// Deletes the character at the cursor, if any.
    pub fn delete(&mut self) {
        if self.cursor >= self.char_len() {
            return;
        }
        let at = self.cursor_byte();
        self.value.remove(at);
        self.history_pos = None;
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

    /// Moves the cursor one word to the left (Ctrl+←).
    ///
    /// Skips trailing whitespace, then skips the preceding word.
    pub fn move_word_left(&mut self) {
        let chars: Vec<char> = self.value.chars().collect();
        let mut i = self.cursor;
        // Skip whitespace to the left.
        while i > 0 && chars[i - 1].is_whitespace() {
            i -= 1;
        }
        // Skip the word characters to the left.
        while i > 0 && !chars[i - 1].is_whitespace() {
            i -= 1;
        }
        self.cursor = i;
    }

    /// Moves the cursor one word to the right (Ctrl+→).
    ///
    /// Skips whitespace, then skips the following word.
    pub fn move_word_right(&mut self) {
        let chars: Vec<char> = self.value.chars().collect();
        let len = chars.len();
        let mut i = self.cursor;
        // Skip any whitespace to the right.
        while i < len && chars[i].is_whitespace() {
            i += 1;
        }
        // Skip the word characters to the right.
        while i < len && !chars[i].is_whitespace() {
            i += 1;
        }
        self.cursor = i;
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
        self.history_pos = None;
    }

    /// Pushes a submitted message onto the history stack.
    ///
    /// Duplicate entries (same text as the most recent entry) are silently
    /// skipped to avoid cluttering the stack with repeated commands.
    /// Entries exceeding [`HISTORY_CAP`] evict the oldest item.
    pub fn push_history(&mut self, text: &str) {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        // Deduplicate against the most recent entry.
        if self.history.last().is_some_and(|h| h == trimmed) {
            return;
        }
        if self.history.len() >= HISTORY_CAP {
            self.history.remove(0);
        }
        self.history.push(trimmed.to_owned());
    }

    /// Navigates one step older in history (↑ in input-history mode).
    ///
    /// Returns `true` when the caller should consume the key and **not** pass it
    /// to the scrollback handler.  Returns `false` when there is no history to
    /// navigate (empty history, or already at the oldest entry).
    ///
    /// Precondition: the caller must check [`Self::should_history_navigate`]
    /// before calling this.
    pub fn history_prev(&mut self) -> bool {
        if self.history.is_empty() {
            return false;
        }
        match self.history_pos {
            None => {
                // First ↑: save draft, jump to the newest entry.
                self.draft = self.value.clone();
                let idx = self.history.len() - 1;
                self.history_pos = Some(idx);
                self.value = self.history[idx].clone();
                self.cursor = self.char_len();
                true
            }
            Some(0) => {
                // Already at the oldest entry — do nothing (caller scrolls back).
                false
            }
            Some(i) => {
                let idx = i - 1;
                self.history_pos = Some(idx);
                self.value = self.history[idx].clone();
                self.cursor = self.char_len();
                true
            }
        }
    }

    /// Navigates one step newer in history (↓ in input-history mode).
    ///
    /// Returns `true` when the key was consumed by history navigation.
    /// Returns `false` when not in history mode (caller scrolls forward).
    pub fn history_next(&mut self) -> bool {
        match self.history_pos {
            None => false,
            Some(i) if i + 1 >= self.history.len() => {
                // Back to the live draft.
                self.history_pos = None;
                self.value = std::mem::take(&mut self.draft);
                self.cursor = self.char_len();
                true
            }
            Some(i) => {
                let idx = i + 1;
                self.history_pos = Some(idx);
                self.value = self.history[idx].clone();
                self.cursor = self.char_len();
                true
            }
        }
    }

    /// `true` when ↑/↓ should navigate history instead of scrollback.
    ///
    /// History navigation is active when the buffer is a single line (no `\n`)
    /// and the cursor is at position 0 or the buffer is empty; once any newline
    /// is present the up/down arrows must navigate within the multi-line text
    /// instead.  When the input is empty, ↑ always enters history mode.
    #[must_use]
    pub fn should_history_navigate(&self) -> bool {
        !self.value.contains('\n')
    }

    /// Clears the buffer and returns its previous contents, trimmed.
    pub fn take(&mut self) -> String {
        self.cursor = 0;
        self.history_pos = None;
        self.draft.clear();
        std::mem::take(&mut self.value).trim().to_owned()
    }

    /// Empties the buffer without returning the contents.
    pub fn clear(&mut self) {
        self.value.clear();
        self.cursor = 0;
        self.history_pos = None;
        self.draft.clear();
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

    // ---- word movement ----

    #[test]
    fn move_word_left_from_end() {
        let mut input = Input::new();
        input.insert_str("hello world");
        input.move_word_left();
        // Should jump to start of "world"
        assert_eq!(input.cursor, 6);
    }

    #[test]
    fn move_word_right_from_start() {
        let mut input = Input::new();
        input.insert_str("hello world");
        input.move_home();
        input.move_word_right();
        // Should land after "hello"
        assert_eq!(input.cursor, 5);
    }

    #[test]
    fn move_word_left_skips_whitespace() {
        let mut input = Input::new();
        input.insert_str("foo   ");
        input.move_word_left();
        // Skips trailing spaces then the word
        assert_eq!(input.cursor, 0);
    }

    #[test]
    fn move_word_right_at_end_is_noop() {
        let mut input = Input::new();
        input.insert_str("hi");
        let pos = input.cursor;
        input.move_word_right();
        assert_eq!(input.cursor, pos);
    }

    // ---- history navigation ----

    #[test]
    fn history_up_down_round_trip() {
        let mut input = Input::new();
        input.push_history("first");
        input.push_history("second");

        // Navigate up to "second"
        assert!(input.history_prev());
        assert_eq!(input.value(), "second");

        // Navigate up again to "first"
        assert!(input.history_prev());
        assert_eq!(input.value(), "first");

        // At oldest, further ↑ returns false
        assert!(!input.history_prev());

        // Navigate back down to "second"
        assert!(input.history_next());
        assert_eq!(input.value(), "second");

        // Navigate back to draft
        assert!(input.history_next());
        assert_eq!(input.value(), ""); // draft was empty
    }

    #[test]
    fn history_draft_is_restored() {
        let mut input = Input::new();
        input.insert_str("my draft");
        input.push_history("old cmd");
        input.history_prev();
        // Now showing "old cmd"
        assert_eq!(input.value(), "old cmd");
        input.history_next();
        // Draft restored
        assert_eq!(input.value(), "my draft");
    }

    #[test]
    fn history_next_when_not_in_history_mode_returns_false() {
        let mut input = Input::new();
        input.push_history("something");
        // Not yet browsing history
        assert!(!input.history_next());
    }

    #[test]
    fn history_prev_empty_returns_false() {
        let mut input = Input::new();
        assert!(!input.history_prev());
    }

    #[test]
    fn history_deduplicates_consecutive() {
        let mut input = Input::new();
        input.push_history("dup");
        input.push_history("dup");
        // Only one entry despite two pushes.
        assert_eq!(input.history.len(), 1);
    }

    #[test]
    fn should_history_navigate_false_when_multiline() {
        let mut input = Input::new();
        input.insert_str("line1\nline2");
        assert!(!input.should_history_navigate());
    }

    #[test]
    fn should_history_navigate_true_for_single_line() {
        let mut input = Input::new();
        input.insert_str("hello");
        assert!(input.should_history_navigate());
    }

    #[test]
    fn history_edit_exits_browse_mode() {
        let mut input = Input::new();
        input.push_history("old");
        input.history_prev();
        assert_eq!(input.value(), "old");
        // Typing drops back to live edit mode
        input.insert_char('!');
        assert!(input.history_pos.is_none());
    }
}

// Rust guideline compliant 2026-02-21
