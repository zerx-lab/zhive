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
    ///
    /// When the cursor sits immediately after a `[Image #N]` token the entire
    /// token is removed atomically, so the user experiences one backspace per
    /// image rather than having to delete each character of the placeholder.
    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        // Check whether the text before the cursor ends with an [Image #N] token.
        let before: String = self.value.chars().take(self.cursor).collect();
        if let Some(token_start) = image_token_end_before(&before) {
            let token_chars = before.chars().count() - token_start;
            let byte_start: usize = self
                .value
                .char_indices()
                .nth(token_start)
                .map_or(self.value.len(), |(b, _)| b);
            self.value.drain(byte_start..self.cursor_byte());
            self.cursor -= token_chars;
        } else {
            self.cursor -= 1;
            let at = self.cursor_byte();
            self.value.remove(at);
        }
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

    /// Replaces the buffer contents with `text`, placing the cursor at the end.
    ///
    /// Used to rewrite the buffer in-place (e.g. stripping image placeholder
    /// tokens when attachments are cleared) without going through history.
    pub(crate) fn set_text(&mut self, text: String) {
        self.cursor = text.chars().count();
        self.value = text;
        self.history_pos = None;
        self.draft.clear();
    }

    /// The cursor's display position as `(column, row)` in cells.
    ///
    /// The row counts hard newlines before the cursor; the column is the total
    /// display width (CJK-aware) of the characters since the last newline. This
    /// ignores soft wrapping — use [`Self::cursor_visual_col_row`] when the
    /// composer wraps long lines to a finite width.
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

    /// Lays the buffer out into visual rows for a composer `width` cells wide.
    ///
    /// Rows break at hard newlines and—when a glyph would overflow the right
    /// edge—at character boundaries, measuring with [`unicode_width`] so wide
    /// CJK glyphs occupy two cells. Character-level wrapping (rather than word
    /// wrapping) keeps cursor tracking exact in [`Self::cursor_visual_col_row`].
    /// An empty buffer yields a single empty row; a `width` of `0` is treated
    /// as `1` to avoid a zero-width division of the text.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_tui::input::Input;
    /// let mut input = Input::new();
    /// input.insert_str("abcdef");
    /// assert_eq!(input.wrap_rows(4), vec!["abcd".to_owned(), "ef".to_owned()]);
    /// ```
    #[must_use]
    pub fn wrap_rows(&self, width: u16) -> Vec<String> {
        let width = usize::from(width.max(1));
        let mut rows: Vec<String> = Vec::new();
        let mut cur = String::new();
        let mut col: usize = 0;
        for c in self.value.chars() {
            if c == '\n' {
                rows.push(std::mem::take(&mut cur));
                col = 0;
                continue;
            }
            let w = UnicodeWidthChar::width(c).unwrap_or(0);
            if col + w > width && !cur.is_empty() {
                rows.push(std::mem::take(&mut cur));
                col = 0;
            }
            cur.push(c);
            col += w;
        }
        rows.push(cur);
        rows
    }

    /// The cursor's visual `(column, row)` when wrapped to `width` cells.
    ///
    /// Applies the same character-level wrapping as [`Self::wrap_rows`] so the
    /// caret aligns with the rendered text. A cursor that exactly fills a row is
    /// reported at the start of the next row, since that is where the next typed
    /// glyph lands. A `width` of `0` is treated as `1`.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_tui::input::Input;
    /// let mut input = Input::new();
    /// input.insert_str("abcd");
    /// // Four chars exactly fill width 4; the caret sits on the next row.
    /// assert_eq!(input.cursor_visual_col_row(4), (0, 1));
    /// ```
    #[must_use]
    pub fn cursor_visual_col_row(&self, width: u16) -> (u16, u16) {
        let width = usize::from(width.max(1));
        let mut col: usize = 0;
        let mut row: usize = 0;
        for c in self.value.chars().take(self.cursor) {
            if c == '\n' {
                row += 1;
                col = 0;
                continue;
            }
            let w = UnicodeWidthChar::width(c).unwrap_or(0);
            if col + w > width && col > 0 {
                row += 1;
                col = 0;
            }
            col += w;
        }
        // A caret that fills the row exactly sits at the start of the next one.
        if col >= width {
            row += 1;
            col = 0;
        }
        (
            u16::try_from(col).unwrap_or(u16::MAX),
            u16::try_from(row).unwrap_or(u16::MAX),
        )
    }
}

/// Returns the character-index start of an `[Image #N]` token that ends exactly
/// at the end of `text`, or `None` if no such token is present.
///
/// Used by [`Input::backspace`] to decide whether to delete a whole token.
fn image_token_end_before(text: &str) -> Option<usize> {
    let text = text.strip_suffix(']')?;
    // Walk backwards over digits for N.
    let digits_end = text.len();
    let text = text.trim_end_matches(|c: char| c.is_ascii_digit());
    if text.len() == digits_end {
        // No digits found.
        return None;
    }
    let text = text.strip_suffix(" #")?;
    let text = text.strip_suffix("[Image")?;
    Some(text.chars().count())
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

    // ---- soft wrapping ----

    #[test]
    fn wrap_rows_breaks_at_width() {
        let mut input = Input::new();
        input.insert_str("abcdefgh");
        assert_eq!(input.wrap_rows(3), vec!["abc", "def", "gh"]);
    }

    #[test]
    fn wrap_rows_respects_hard_newlines() {
        let mut input = Input::new();
        input.insert_str("ab\ncd");
        assert_eq!(input.wrap_rows(10), vec!["ab", "cd"]);
    }

    #[test]
    fn wrap_rows_keeps_wide_glyph_whole_at_boundary() {
        let mut input = Input::new();
        // Width 2: "a" leaves one free cell, but the 2-cell 你 cannot split, so
        // it wraps whole to the next row instead of spilling across the edge.
        input.insert_str("a你");
        assert_eq!(input.wrap_rows(2), vec!["a", "你"]);
    }

    #[test]
    fn cursor_visual_wraps_to_next_row() {
        let mut input = Input::new();
        input.insert_str("abcdef");
        input.move_home();
        for _ in 0..4 {
            input.move_right();
        }
        // Cursor after 4 chars at width 4 lands at the start of the next row.
        assert_eq!(input.cursor_visual_col_row(4), (0, 1));
    }

    #[test]
    fn cursor_visual_mid_wrapped_line() {
        let mut input = Input::new();
        input.insert_str("abcdef");
        // Cursor at end: 6 chars over width 4 → row 1, column 2.
        assert_eq!(input.cursor_visual_col_row(4), (2, 1));
    }

    #[test]
    fn take_trims_and_clears() {
        let mut input = Input::new();
        input.insert_str("  hi  ");
        assert_eq!(input.take(), "hi");
        assert!(input.is_blank());
    }

    #[test]
    fn backspace_deletes_image_token_atomically() {
        let mut input = Input::new();
        input.insert_str("hello[Image #1]");
        // One backspace must remove the entire token, not just ']'.
        input.backspace();
        assert_eq!(input.value(), "hello");
        // Backspace on plain text still removes one char.
        input.backspace();
        assert_eq!(input.value(), "hell");
    }

    #[test]
    fn backspace_image_token_mid_text() {
        let mut input = Input::new();
        input.insert_str("[Image #1]世界");
        // Move cursor just past the token (before '世').
        input.move_home();
        // advance past ']'
        for _ in 0..10 {
            input.move_right();
        }
        input.backspace();
        assert_eq!(input.value(), "世界");
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
