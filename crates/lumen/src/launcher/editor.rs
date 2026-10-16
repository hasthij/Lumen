//! Single-line text editing for the search box: caret, selection, word jumps, clipboard.

#[derive(Debug, Default, Clone)]
pub struct Editor {
    chars: Vec<char>,
    pub caret: usize,
    /// Selection anchor; the selection spans anchor..caret.
    pub anchor: Option<usize>,
}

impl Editor {
    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    pub fn len(&self) -> usize {
        self.chars.len()
    }

    pub fn set(&mut self, s: &str) {
        self.chars = s.chars().collect();
        self.caret = self.chars.len();
        self.anchor = None;
    }

    pub fn selection(&self) -> Option<(usize, usize)> {
        let a = self.anchor?;
        (a != self.caret).then(|| (a.min(self.caret), a.max(self.caret)))
    }

    pub fn selected_text(&self) -> Option<String> {
        self.selection().map(|(a, b)| self.chars[a..b].iter().collect())
    }

    pub fn select_all(&mut self) {
        self.anchor = Some(0);
        self.caret = self.chars.len();
    }

    fn delete_selection(&mut self) -> bool {
        match self.selection() {
            Some((a, b)) => {
                self.chars.drain(a..b);
                self.caret = a;
                self.anchor = None;
                true
            }
            None => {
                self.anchor = None;
                false
            }
        }
    }

    pub fn insert(&mut self, s: &str) {
        self.delete_selection();
        // Single line: newlines and tabs from pasted text become spaces.
        let new: Vec<char> = s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
        let n = new.len();
        self.chars.splice(self.caret..self.caret, new);
        self.caret += n;
    }

    pub fn backspace(&mut self, word: bool) {
        if self.delete_selection() || self.caret == 0 {
            return;
        }
        let start = if word { self.word_left(self.caret) } else { self.caret - 1 };
        self.chars.drain(start..self.caret);
        self.caret = start;
    }

    pub fn delete(&mut self, word: bool) {
        if self.delete_selection() || self.caret >= self.chars.len() {
            return;
        }
        let end = if word { self.word_right(self.caret) } else { self.caret + 1 };
        self.chars.drain(self.caret..end);
    }

    pub fn move_to(&mut self, pos: usize, extend: bool) {
        if extend {
            self.anchor.get_or_insert(self.caret);
        } else {
            self.anchor = None;
        }
        self.caret = pos.min(self.chars.len());
    }

    pub fn left(&mut self, word: bool, extend: bool) {
        if !extend {
            if let Some((a, _)) = self.selection() {
                return self.move_to(a, false);
            }
        }
        let p = if word { self.word_left(self.caret) } else { self.caret.saturating_sub(1) };
        self.move_to(p, extend);
    }

    pub fn right(&mut self, word: bool, extend: bool) {
        if !extend {
            if let Some((_, b)) = self.selection() {
                return self.move_to(b, false);
            }
        }
        let p = if word { self.word_right(self.caret) } else { self.caret + 1 };
        self.move_to(p, extend);
    }

    fn word_left(&self, mut i: usize) -> usize {
        while i > 0 && !self.chars[i - 1].is_alphanumeric() {
            i -= 1;
        }
        while i > 0 && self.chars[i - 1].is_alphanumeric() {
            i -= 1;
        }
        i
    }

    fn word_right(&self, mut i: usize) -> usize {
        let n = self.chars.len();
        while i < n && self.chars[i].is_alphanumeric() {
            i += 1;
        }
        while i < n && !self.chars[i].is_alphanumeric() {
            i += 1;
        }
        i
    }

    /// UTF-16 index of a char index, for DirectWrite hit testing.
    pub fn utf16_index(&self, i: usize) -> u32 {
        self.chars[..i.min(self.chars.len())].iter().map(|c| c.len_utf16() as u32).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editing() {
        let mut e = Editor::default();
        e.insert("hello world");
        e.backspace(true);
        assert_eq!(e.text(), "hello ");
        e.select_all();
        e.insert("x");
        assert_eq!(e.text(), "x");
        e.insert("yz");
        e.left(false, true);
        assert_eq!(e.selected_text().as_deref(), Some("z"));
        e.backspace(false);
        assert_eq!(e.text(), "xy");
    }
}
