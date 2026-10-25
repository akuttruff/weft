//! The text being typed, with a cursor that can move through it.

use std::ops::Deref;

use unicode_width::UnicodeWidthStr;

/// What is in the input box and where the cursor is, as a character index.
// Rust: `Deref<Target = str>` lets the rest of the screen call `is_empty`,
// `starts_with`, `trim` and so on straight on an `Input`, as if it were the
// string; only changes have to go through the methods below.
#[derive(Debug, Default, Clone)]
pub struct Input {
    text: String,
    cursor: usize,
}

impl Deref for Input {
    type Target = str;
    fn deref(&self) -> &str {
        &self.text
    }
}

impl Input {
    pub fn set(&mut self, text: String) {
        self.cursor = text.chars().count();
        self.text = text;
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    pub fn insert(&mut self, c: char) {
        let at = self.byte_at(self.cursor);
        self.text.insert(at, c);
        self.cursor += 1;
    }

    pub fn insert_str(&mut self, s: &str) {
        let at = self.byte_at(self.cursor);
        self.text.insert_str(at, s);
        self.cursor += s.chars().count();
    }

    /// Delete the character before the cursor.
    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            let from = self.byte_at(self.cursor - 1);
            let to = self.byte_at(self.cursor);
            self.text.replace_range(from..to, "");
            self.cursor -= 1;
        }
    }

    /// Delete the character under the cursor.
    pub fn delete(&mut self) {
        if self.cursor < self.len_chars() {
            let from = self.byte_at(self.cursor);
            let to = self.byte_at(self.cursor + 1);
            self.text.replace_range(from..to, "");
        }
    }

    /// Delete the word before the cursor, and the spaces after it.
    pub fn delete_word(&mut self) {
        let to = self.byte_at(self.cursor);
        let head = &self.text[..to];
        let trimmed = head.trim_end();
        let cut = trimmed
            .rfind(char::is_whitespace)
            .map(|i| i + 1)
            .unwrap_or(0);
        let removed = head[cut..].chars().count();
        self.text.replace_range(cut..to, "");
        self.cursor -= removed;
    }

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.len_chars());
    }

    /// To the start of the current line.
    pub fn home(&mut self) {
        let at = self.byte_at(self.cursor);
        let start = self.text[..at].rfind('\n').map(|i| i + 1).unwrap_or(0);
        self.cursor = self.text[..start].chars().count();
    }

    /// To the end of the current line.
    pub fn end(&mut self) {
        let at = self.byte_at(self.cursor);
        let end = self.text[at..]
            .find('\n')
            .map(|i| at + i)
            .unwrap_or(self.text.len());
        self.cursor = self.text[..end].chars().count();
    }

    /// Where the cursor sits once the text is wrapped at `width`, as (row,
    /// column), with the `> ` prefix counted on the first row and two spaces
    /// on the others.
    pub fn cursor_at(&self, width: u16) -> (u16, u16) {
        let width = width.max(3) as usize;
        let before: String = self.text.chars().take(self.cursor).collect();
        let mut row = 0usize;
        let mut col = 0usize;
        for (i, seg) in before.split('\n').enumerate() {
            if i > 0 {
                row += 1;
            }
            let len = 2 + seg.width();
            row += len / width;
            col = len % width;
        }
        (row as u16, col as u16)
    }

    fn len_chars(&self) -> usize {
        self.text.chars().count()
    }

    /// The byte offset of character `n`.
    fn byte_at(&self, n: usize) -> usize {
        self.text
            .char_indices()
            .nth(n)
            .map(|(i, _)| i)
            .unwrap_or(self.text.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_happen_at_the_cursor() {
        let mut input = Input::default();
        input.insert_str("héllo world");
        input.left();
        input.left();
        input.left();
        input.left();
        input.left();
        input.left(); // before the space
        input.insert(',');
        assert_eq!(&*input, "héllo, world");
        input.home();
        input.delete();
        assert_eq!(&*input, "éllo, world");
        input.end();
        input.backspace();
        assert_eq!(&*input, "éllo, worl");
        input.delete_word();
        assert_eq!(&*input, "éllo, ");
        input.set("a\nbc".into());
        input.left();
        input.home();
        input.insert('>');
        assert_eq!(&*input, "a\n>bc");
    }

    #[test]
    fn cursor_follows_wrapping_and_newlines() {
        let mut input = Input::default();
        assert_eq!(input.cursor_at(20), (0, 2));
        input.set("abc".into());
        assert_eq!(input.cursor_at(20), (0, 5));
        input.set("a\nbc".into());
        assert_eq!(input.cursor_at(20), (1, 4));
        input.set("x".repeat(18));
        assert_eq!(input.cursor_at(20), (1, 0));
        input.set("x".repeat(19));
        assert_eq!(input.cursor_at(20), (1, 1));
        input.home();
        assert_eq!(input.cursor_at(20), (0, 2));
    }
}
