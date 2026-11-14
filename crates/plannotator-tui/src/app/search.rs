//! `/` and `?` search over the open document's text.
//!
//! A plain case-sensitive substring search of `open.doc.source`. The prompt collects a
//! query on the footer line; `/` searches forward and `?` backward. Enter finds every
//! occurrence and jumps to the first in the search direction from the cursor, then
//! `search-next` / `search-prev` cycle through them (next follows the search direction,
//! prev reverses it, as in `less`). The matches are byte offsets into the source, so a jump
//! reuses the same block-and-row mapping annotations use.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::compose::{Compose, ComposeAction};
use super::{App, Mode};

impl App {
    /// Open the search prompt. `backward` chooses `?` (up) over `/` (down); the sigil the
    /// footer draws and which match Enter lands on both follow it.
    pub(super) fn begin_search(&mut self, backward: bool) {
        self.search = Compose::default();
        self.search_backward = backward;
        self.status = None;
        self.mode = Mode::Search;
    }

    /// Clear the active search: its matches and highlighting go, the cursor stays. Esc in
    /// the document calls this before it would quit, so a search cancels cleanly.
    pub(super) fn clear_search(&mut self) {
        self.search_matches.clear();
        self.search_at = 0;
        self.status = Some("search cleared".into());
    }

    /// The sigil the prompt shows: `/` forward, `?` backward.
    pub(super) fn search_sigil(&self) -> char {
        if self.search_backward { '?' } else { '/' }
    }

    /// Keys in the search prompt: Enter searches and jumps, Esc cancels, the rest edit the
    /// one line. A query spans one line, so the comment box's newline key does nothing.
    pub(super) fn search_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => self.submit_search(),
            KeyCode::Char('j' | 'J') if key.modifiers.contains(KeyModifiers::CONTROL) => {}
            _ => {
                if self.search.handle_key(key) == ComposeAction::Cancel {
                    self.mode = Mode::Browse;
                }
            }
        }
    }

    /// A paste joins the query; line breaks are dropped.
    pub(super) fn search_paste(&mut self, text: &str) {
        self.search.insert_text(&text.replace(['\r', '\n'], ""));
    }

    /// Run the typed query. An empty query closes the prompt and leaves any earlier match
    /// state alone; a query with no match says so and keeps the cursor where it is.
    fn submit_search(&mut self) {
        let query = self.search.value();
        self.mode = Mode::Browse;
        if query.is_empty() {
            return;
        }
        let matches: Vec<usize> = byte_offsets(&self.open.doc.source, &query);
        if matches.is_empty() {
            self.search_matches.clear();
            self.search_query = query.clone();
            self.status = Some(format!("no match for {query:?}"));
            return;
        }
        // The first match in the search direction from the cursor, so Enter moves from
        // where the reader is: forward lands on the first at or after the cursor, backward
        // on the last at or before it. Each wraps when there is none that way.
        let from = self.cursor_byte();
        let at = if self.search_backward {
            matches.iter().rposition(|&off| off <= from).unwrap_or(matches.len() - 1)
        } else {
            matches.iter().position(|&off| off >= from).unwrap_or(0)
        };
        self.search_query = query;
        self.search_matches = matches;
        self.search_at = at;
        self.jump_to_current_match();
    }

    /// `search-next` / `search-prev`: step through the matches of the last query, wrapping
    /// at the ends. `next` follows the search direction and `prev` reverses it, so under a
    /// `?` search `n` goes up and `N` goes down, as in `less`. With no live query it
    /// re-opens the prompt so `n` is still useful.
    pub(super) fn search_step(&mut self, next: bool) {
        if self.search_matches.is_empty() {
            if self.search_query.is_empty() {
                self.begin_search(false);
            } else {
                self.status = Some(format!("no match for {:?}", self.search_query));
            }
            return;
        }
        let forward = next != self.search_backward;
        let len = self.search_matches.len();
        self.search_at = if forward {
            (self.search_at + 1) % len
        } else {
            (self.search_at + len - 1) % len
        };
        self.jump_to_current_match();
    }

    /// Select the block holding the current match and put the cursor on it, scrolled into
    /// view. The footer reports which match this is.
    fn jump_to_current_match(&mut self) {
        let Some(&start) = self.search_matches.get(self.search_at) else { return };
        let len = self.search_query.len();
        let range = start..start + len;
        if let Some(block) = self.open.doc.block_containing(start) {
            self.select_block(block);
            if let Some(row) = self.open.layout.first_row_in_range(block, &range) {
                let col = self.match_column(row, start);
                self.cursor = (row, col);
                self.ensure_cursor_visible();
            }
        }
        self.status = Some(format!(
            "match {}/{} for {:?}",
            self.search_at + 1,
            self.search_matches.len(),
            self.search_query
        ));
    }

    /// The screen column in `row` whose cell maps to source byte `offset`, else 0.
    fn match_column(&self, row: usize, offset: usize) -> usize {
        self.open
            .layout
            .row(row)
            .and_then(|r| r.cells.iter().position(|cell| *cell == Some(offset)))
            .unwrap_or(0)
    }

    /// The source byte under the cursor, for choosing which match Enter jumps to. Falls
    /// back to the selected block's start when the cursor cell carries no offset (a gap
    /// row or decoration).
    fn cursor_byte(&self) -> usize {
        let (row, col) = self.cursor;
        if let Some(Some(offset)) = self.open.layout.row(row).map(|r| r.cells.get(col).copied().flatten()) {
            return offset;
        }
        self.open.doc.blocks.get(self.selected).map_or(0, |b| b.range.start)
    }
}

/// The start byte offset of every occurrence of `needle` in `haystack`, left to right and
/// non-overlapping.
fn byte_offsets(haystack: &str, needle: &str) -> Vec<usize> {
    if needle.is_empty() {
        return Vec::new();
    }
    let mut offsets = Vec::new();
    let mut from = 0;
    while let Some(at) = haystack[from..].find(needle) {
        let start = from + at;
        offsets.push(start);
        from = start + needle.len();
        if from > haystack.len() {
            break;
        }
    }
    offsets
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_every_non_overlapping_occurrence() {
        assert_eq!(byte_offsets("abcabc", "bc"), vec![1, 4]);
        assert_eq!(byte_offsets("aaaa", "aa"), vec![0, 2]);
        assert_eq!(byte_offsets("abc", "x"), Vec::<usize>::new());
        assert_eq!(byte_offsets("abc", ""), Vec::<usize>::new());
    }

    #[test]
    fn offsets_are_utf8_byte_positions() {
        // "é" is two bytes, so "x" after it starts at byte 2.
        assert_eq!(byte_offsets("éx", "x"), vec![2]);
    }
}
