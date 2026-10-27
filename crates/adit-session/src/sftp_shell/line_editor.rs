#[derive(Default)]
pub(super) struct LineEditor {
    pub line: String,
    pub cursor: usize,
    history: Vec<String>,
    position: Option<usize>,
    draft: String,
    search: Option<Search>,
}

struct Search {
    query: String,
    found: Option<usize>,
    saved: String,
    cursor: usize,
}

impl LineEditor {
    pub fn insert(&mut self, ch: char) {
        if let Some(search) = &mut self.search {
            search.query.push(ch);
            self.find(false);
        } else {
            self.line.insert(self.cursor, ch);
            self.cursor += ch.len_utf8();
        }
    }

    pub fn backspace(&mut self) {
        if let Some(search) = &mut self.search {
            search.query.pop();
            self.find(false);
        } else if self.cursor > 0 {
            let previous = self.line[..self.cursor]
                .char_indices()
                .next_back()
                .map_or(0, |(i, _)| i);
            self.line.drain(previous..self.cursor);
            self.cursor = previous;
        }
    }

    pub fn delete(&mut self) {
        if let Some(ch) = self.line[self.cursor..].chars().next() {
            self.line.drain(self.cursor..self.cursor + ch.len_utf8());
        }
    }

    pub fn left(&mut self) {
        self.accept_search();
        self.cursor = self.line[..self.cursor]
            .char_indices()
            .next_back()
            .map_or(0, |(i, _)| i);
    }

    pub fn right(&mut self) {
        self.accept_search();
        if let Some(ch) = self.line[self.cursor..].chars().next() {
            self.cursor += ch.len_utf8();
        }
    }

    pub fn home(&mut self) {
        self.accept_search();
        self.cursor = 0;
    }
    pub fn end(&mut self) {
        self.accept_search();
        self.cursor = self.line.len();
    }
    pub fn kill_before(&mut self) {
        self.accept_search();
        self.line.drain(..self.cursor);
        self.cursor = 0;
    }
    pub fn kill_after(&mut self) {
        self.accept_search();
        self.line.truncate(self.cursor);
    }

    pub fn kill_word(&mut self) {
        self.accept_search();
        let mut start = self.cursor;
        while start > 0 {
            let (index, ch) = self.line[..start]
                .char_indices()
                .next_back()
                .expect("nonempty");
            if !ch.is_whitespace() {
                break;
            }
            start = index;
        }
        while start > 0 {
            let (index, ch) = self.line[..start]
                .char_indices()
                .next_back()
                .expect("nonempty");
            if ch.is_whitespace() {
                break;
            }
            start = index;
        }
        self.line.drain(start..self.cursor);
        self.cursor = start;
    }

    pub fn recall(&mut self, older: bool) {
        self.accept_search();
        if self.history.is_empty() || (self.position.is_none() && !older) {
            return;
        }
        self.position = match (self.position, older) {
            (None, true) => {
                self.draft = self.line.clone();
                Some(self.history.len() - 1)
            }
            (Some(index), true) => Some(index.saturating_sub(1)),
            (Some(index), false) if index + 1 < self.history.len() => Some(index + 1),
            _ => None,
        };
        self.line = self
            .position
            .map_or_else(|| self.draft.clone(), |i| self.history[i].clone());
        self.cursor = self.line.len();
    }

    pub fn reverse_search(&mut self) {
        let again = self.search.is_some();
        if !again {
            self.search = Some(Search {
                query: String::new(),
                found: None,
                saved: self.line.clone(),
                cursor: self.cursor,
            });
        }
        self.find(again);
    }

    fn find(&mut self, older: bool) {
        let Some(search) = &mut self.search else {
            return;
        };
        let end = if older {
            search.found.unwrap_or(0)
        } else {
            self.history.len()
        };
        let found = (0..end)
            .rev()
            .find(|&i| self.history[i].contains(&search.query));
        search.found = found;
        self.line = found.map_or_else(|| search.saved.clone(), |i| self.history[i].clone());
        self.cursor = self.line.len();
    }

    pub fn search_caption(&self) -> Option<String> {
        self.search.as_ref().map(|s| {
            format!(
                "({}reverse-i-search)`{}': ",
                if s.found.is_none() { "failed " } else { "" },
                s.query,
            )
        })
    }

    /// Enter accepts a match for editing; it never executes a search result.
    pub fn accept_search(&mut self) -> bool {
        if let Some(search) = self.search.take() {
            if let Some(found) = search.found {
                if self.position.is_none() {
                    self.draft = search.saved;
                }
                self.position = Some(found);
            }
            true
        } else {
            false
        }
    }

    pub fn cancel_search(&mut self) {
        if let Some(search) = self.search.take() {
            self.line = search.saved;
            self.cursor = search.cursor;
        }
    }

    pub fn clear(&mut self) {
        self.search = None;
        self.line.clear();
        self.cursor = 0;
        self.position = None;
        self.draft.clear();
    }

    pub fn submit(&mut self) -> String {
        let line = self.line.trim().to_string();
        if !line.is_empty() && self.history.last() != Some(&line) {
            self.history.push(line.clone());
            if self.history.len() > 1000 {
                self.history.remove(0);
            }
        }
        self.clear();
        line
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn type_line(editor: &mut LineEditor, text: &str) {
        for ch in text.chars() {
            editor.insert(ch);
        }
    }

    #[test]
    fn middle_editing_keeps_unicode_boundaries_and_suffix() {
        let mut editor = LineEditor::default();
        type_line(&mut editor, "get 文件.txt");
        editor.left();
        editor.left();
        editor.left();
        editor.left();
        editor.insert('新');
        assert_eq!(editor.line, "get 文件新.txt");
        editor.backspace();
        editor.delete();
        assert_eq!(editor.line, "get 文件txt");
        editor.home();
        editor.right();
        editor.kill_after();
        assert_eq!(editor.line, "g");
    }

    #[test]
    fn reverse_search_cycles_filters_accepts_without_submission_and_cancels() {
        let mut editor = LineEditor::default();
        for command in ["get old", "put file", "get new"] {
            type_line(&mut editor, command);
            editor.submit();
        }
        type_line(&mut editor, "draft");
        editor.left();
        editor.reverse_search();
        type_line(&mut editor, "get");
        assert_eq!(editor.line, "get new");
        editor.reverse_search();
        assert_eq!(editor.line, "get old");
        editor.cancel_search();
        assert_eq!((&*editor.line, editor.cursor), ("draft", 4));
        editor.reverse_search();
        type_line(&mut editor, "missing");
        assert!(editor.search_caption().unwrap().contains("failed"));
        editor.cancel_search();
        editor.reverse_search();
        assert!(editor.accept_search());
        assert_eq!(editor.line, "get new");
        assert_eq!(editor.history.len(), 3);
    }

    #[test]
    fn history_restores_draft_and_control_word_edits() {
        let mut editor = LineEditor::default();
        type_line(&mut editor, "ls /");
        editor.submit();
        type_line(&mut editor, "draft text");
        editor.recall(false);
        assert_eq!(
            editor.line, "draft text",
            "Down must not erase an untouched draft"
        );
        editor.recall(true);
        editor.recall(false);
        assert_eq!(editor.line, "draft text");
        editor.kill_word();
        assert_eq!(editor.line, "draft ");
        editor.kill_before();
        assert!(editor.line.is_empty());
    }
}
