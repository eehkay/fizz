//! The message compose box: a small multi-line text editor.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Default, Clone)]
pub struct Input {
    text: String,
    /// Byte offset of the cursor, always on a character boundary.
    cursor: usize,
}

impl Input {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn set(&mut self, text: String) {
        self.cursor = text.len();
        self.text = text;
    }

    pub fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.text)
    }

    pub fn insert(&mut self, c: char) {
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    pub fn insert_str(&mut self, s: &str) {
        let s = s.replace("\r\n", "\n").replace('\r', "\n");
        self.text.insert_str(self.cursor, &s);
        self.cursor += s.len();
    }

    // Movement and deletion go by what looks like one character on screen,
    // so an emoji made of several code points is never cut in half.
    fn prev(&self) -> Option<usize> {
        self.text[..self.cursor]
            .grapheme_indices(true)
            .next_back()
            .map(|(i, _)| i)
    }

    fn next(&self) -> Option<usize> {
        self.text[self.cursor..]
            .graphemes(true)
            .next()
            .map(|g| self.cursor + g.len())
    }

    /// Where a partly typed `:name` before the cursor starts, and the name.
    fn emoji_span(&self) -> Option<(usize, &str)> {
        let before = &self.text[..self.cursor];
        let colon = before.rfind(':')?;
        let name = &before[colon + 1..];
        let named = name.len() >= 2
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'+' || b == b'-');
        // Only at the start of a word, so "10:30" and "http://" are left alone.
        let at_word_start = before[..colon]
            .chars()
            .next_back()
            .is_none_or(char::is_whitespace);
        (named && at_word_start).then_some((colon, name))
    }

    /// The emoji name being typed, if the cursor sits right after `:name`.
    pub fn emoji_query(&self) -> Option<&str> {
        self.emoji_span().map(|(_, name)| name)
    }

    /// Swap the `:name` before the cursor for an emoji.
    pub fn complete_emoji(&mut self, emoji: &str) {
        if let Some((start, _)) = self.emoji_span() {
            self.text.replace_range(start..self.cursor, emoji);
            self.cursor = start + emoji.len();
        }
    }

    pub fn backspace(&mut self) {
        if let Some(p) = self.prev() {
            self.text.drain(p..self.cursor);
            self.cursor = p;
        }
    }

    pub fn delete(&mut self) {
        if let Some(n) = self.next() {
            self.text.drain(self.cursor..n);
        }
    }

    pub fn left(&mut self) {
        if let Some(p) = self.prev() {
            self.cursor = p;
        }
    }

    pub fn right(&mut self) {
        if let Some(n) = self.next() {
            self.cursor = n;
        }
    }

    pub fn home(&mut self) {
        self.cursor = self.text[..self.cursor].rfind('\n').map_or(0, |i| i + 1);
    }

    pub fn end(&mut self) {
        self.cursor = self.text[self.cursor..]
            .find('\n')
            .map_or(self.text.len(), |i| self.cursor + i);
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    pub fn delete_word(&mut self) {
        let before = &self.text[..self.cursor];
        let trimmed = before.trim_end_matches(|c: char| c.is_whitespace());
        let start = trimmed
            .rfind(|c: char| c.is_whitespace())
            .map_or(0, |i| i + trimmed[i..].chars().next().map_or(1, char::len_utf8));
        self.text.drain(start..self.cursor);
        self.cursor = start;
    }

    /// Where each line on screen starts and ends in the text, for a box
    /// `width` cells wide. A line ends at a line break or where it wraps.
    fn rows(&self, width: u16) -> Vec<(usize, usize)> {
        let width = width.max(1) as usize;
        let mut rows = Vec::new();
        let (mut start, mut col) = (0usize, 0usize);
        for (i, g) in self.text.grapheme_indices(true) {
            if g == "\n" || g == "\r\n" {
                rows.push((start, i));
                start = i + g.len();
                col = 0;
                continue;
            }
            let w = g.width();
            if col + w > width {
                rows.push((start, i));
                start = i;
                col = 0;
            }
            col += w;
        }
        rows.push((start, self.text.len()));
        rows
    }

    /// Move the cursor one line up or down, keeping its column where the
    /// line is long enough. Off the first or last line it goes to the very
    /// start or end, as text boxes usually do.
    fn vertical(&mut self, width: u16, down: bool) {
        let rows = self.rows(width);
        // The line the cursor is on: the last one starting at or before it.
        let current = rows
            .iter()
            .rposition(|(start, _)| *start <= self.cursor)
            .unwrap_or(0);
        let column = self.text[rows[current].0..self.cursor].width();
        let target = if down {
            current + 1
        } else {
            match current.checked_sub(1) {
                Some(above) => above,
                None => return self.cursor = 0,
            }
        };
        let Some(&(start, end)) = rows.get(target) else {
            return self.cursor = self.text.len();
        };
        let mut at = start;
        let mut col = 0;
        for g in self.text[start..end].graphemes(true) {
            let w = g.width();
            if col + w > column {
                break;
            }
            col += w;
            at += g.len();
        }
        self.cursor = at;
    }

    pub fn up(&mut self, width: u16) {
        self.vertical(width, false);
    }

    pub fn down(&mut self, width: u16) {
        self.vertical(width, true);
    }

    /// To the start of the word before the cursor.
    pub fn word_left(&mut self) {
        let before = &self.text[..self.cursor];
        let trimmed = before.trim_end_matches(|c: char| c.is_whitespace());
        self.cursor = trimmed
            .rfind(|c: char| c.is_whitespace())
            .map_or(0, |i| i + trimmed[i..].chars().next().map_or(1, char::len_utf8));
    }

    /// To the end of the word after the cursor.
    pub fn word_right(&mut self) {
        let after = &self.text[self.cursor..];
        let skipped = after.len() - after.trim_start_matches(|c: char| c.is_whitespace()).len();
        let word = after[skipped..]
            .find(|c: char| c.is_whitespace())
            .unwrap_or(after.len() - skipped);
        self.cursor += skipped + word;
    }

    /// Lay the text out for a box `width` cells wide. Returns the visual
    /// lines and the cursor's (column, row) within them.
    pub fn layout(&self, width: u16) -> (Vec<String>, (u16, u16)) {
        let width = width.max(1) as usize;
        let mut lines = vec![String::new()];
        let mut col = 0usize;
        let mut cursor = (0u16, 0u16);
        let mut placed = false;
        for (i, g) in self.text.grapheme_indices(true) {
            if i == self.cursor {
                cursor = (col as u16, (lines.len() - 1) as u16);
                placed = true;
            }
            if g == "\n" || g == "\r\n" {
                lines.push(String::new());
                col = 0;
                continue;
            }
            let w = g.width();
            if col + w > width {
                lines.push(String::new());
                col = 0;
                if i == self.cursor {
                    cursor = (0, (lines.len() - 1) as u16);
                }
            }
            lines.last_mut().unwrap().push_str(g);
            col += w;
        }
        if !placed {
            if col >= width {
                lines.push(String::new());
                col = 0;
            }
            cursor = (col as u16, (lines.len() - 1) as u16);
        }
        (lines, cursor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_and_wraps() {
        let mut input = Input::default();
        input.insert_str("hello world");
        input.delete_word();
        assert_eq!(input.text(), "hello ");
        input.insert_str("héllo\nx");
        let (lines, cursor) = input.layout(5);
        assert_eq!(lines, vec!["hello", " héll", "o", "x"]);
        assert_eq!(cursor, (1, 3));
        input.backspace();
        input.backspace();
        input.backspace();
        assert_eq!(input.text(), "hello héll");
    }

    #[test]
    fn completes_emoji_names() {
        let mut input = Input::default();
        input.insert_str("on my way :thu");
        assert_eq!(input.emoji_query(), Some("thu"));
        input.complete_emoji("👍");
        assert_eq!(input.text(), "on my way 👍");
        input.insert_str(" at 10:30");
        assert_eq!(input.emoji_query(), None);
        input.set("see http://ex".into());
        assert_eq!(input.emoji_query(), None);
    }

    #[test]
    fn treats_joined_emoji_as_one() {
        let mut input = Input::default();
        input.insert_str("a👨‍👩‍👧b");
        input.left();
        input.backspace();
        assert_eq!(input.text(), "ab");
    }

    #[test]
    fn moves_between_lines() {
        let mut input = Input::default();
        // Three lines on screen at width 10: a wrap, then a real line break.
        input.insert_str("0123456789abcde\nxy");
        assert_eq!(input.rows(10), vec![(0, 10), (10, 15), (16, 18)]);
        // From the end ("xy|"), up lands at column 2 of "abcde".
        input.up(10);
        assert_eq!(input.cursor, 12);
        input.up(10);
        assert_eq!(input.cursor, 2);
        // Up from the first line goes to the very start.
        input.up(10);
        assert_eq!(input.cursor, 0);
        input.down(10);
        assert_eq!(input.cursor, 10);
        input.down(10);
        assert_eq!(input.cursor, 16);
        // Down from the last line goes to the very end.
        input.down(10);
        assert_eq!(input.cursor, 18);
        // A shorter line clamps the column.
        input.set("long first line\nab\nlong third line".into());
        input.up(40);
        assert_eq!(&input.text[..input.cursor], "long first line\nab");
    }

    #[test]
    fn moves_by_word() {
        let mut input = Input::default();
        input.insert_str("see you  tomorrow");
        input.word_left();
        assert_eq!(&input.text[input.cursor..], "tomorrow");
        input.word_left();
        assert_eq!(&input.text[input.cursor..], "you  tomorrow");
        input.word_right();
        assert_eq!(&input.text[..input.cursor], "see you");
        input.word_right();
        assert_eq!(input.cursor, input.text.len());
    }

    #[test]
    fn cursor_wraps_at_full_line() {
        let mut input = Input::default();
        input.insert_str("abcde");
        let (lines, cursor) = input.layout(5);
        assert_eq!(lines.len(), 2);
        assert_eq!(cursor, (0, 1));
    }
}
