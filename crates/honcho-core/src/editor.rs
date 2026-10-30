use std::ops::Range;

use ropey::Rope;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthChar;

const UNDO_LIMIT: usize = 1000;

/// A selection in char offsets. `anchor == head` is a plain cursor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    pub anchor: usize,
    pub head: usize,
    /// Display column to aim for when moving vertically.
    pub goal: Option<usize>,
}

impl Selection {
    pub fn cursor(at: usize) -> Self {
        Self::new(at, at)
    }

    pub fn new(anchor: usize, head: usize) -> Self {
        Self {
            anchor,
            head,
            goal: None,
        }
    }

    pub fn start(&self) -> usize {
        self.anchor.min(self.head)
    }

    pub fn end(&self) -> usize {
        self.anchor.max(self.head)
    }

    pub fn range(&self) -> Range<usize> {
        self.start()..self.end()
    }

    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }

    pub fn reversed(&self) -> bool {
        self.head < self.anchor
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Motion {
    Left,
    Right,
    Up,
    Down,
    WordLeft,
    WordRight,
    LineStart,
    LineEnd,
    DocStart,
    DocEnd,
    PageUp(usize),
    PageDown(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EditKind {
    Insert,
    Delete,
    Other,
}

struct Snapshot {
    text: Rope,
    selections: Vec<Selection>,
    newest: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CharClass {
    Space,
    Word,
    Punct,
}

fn class(c: char) -> CharClass {
    if c.is_whitespace() {
        CharClass::Space
    } else if c.is_alphanumeric() || c == '_' || c == '$' || c == '@' || c == '#' {
        CharClass::Word
    } else {
        CharClass::Punct
    }
}

fn is_word_char(c: char) -> bool {
    class(c) == CharClass::Word
}

fn closing_pair(c: char) -> Option<char> {
    match c {
        '(' => Some(')'),
        '[' => Some(']'),
        '{' => Some('}'),
        '\'' => Some('\''),
        '"' => Some('"'),
        '`' => Some('`'),
        _ => None,
    }
}

fn is_closing(c: char) -> bool {
    matches!(c, ')' | ']' | '}' | '\'' | '"' | '`')
}

pub struct Editor {
    text: Rope,
    selections: Vec<Selection>,
    newest: usize,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    last_edit: Option<EditKind>,
    version: u64,
    dirty_from_line: Option<usize>,
    /// Cmd+D started from an empty cursor matches whole words only.
    whole_word_search: bool,
    pub tab_size: usize,
}

impl Default for Editor {
    fn default() -> Self {
        Self::new("")
    }
}

impl Editor {
    pub fn new(text: &str) -> Self {
        Self {
            text: Rope::from_str(text),
            selections: vec![Selection::cursor(0)],
            newest: 0,
            undo: Vec::new(),
            redo: Vec::new(),
            last_edit: None,
            version: 0,
            dirty_from_line: Some(0),
            whole_word_search: false,
            tab_size: 4,
        }
    }

    pub fn rope(&self) -> &Rope {
        &self.text
    }

    pub fn text(&self) -> String {
        self.text.to_string()
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    /// First line whose highlighting may be stale since the last call.
    pub fn take_dirty_from_line(&mut self) -> Option<usize> {
        self.dirty_from_line.take()
    }

    pub fn len_chars(&self) -> usize {
        self.text.len_chars()
    }

    pub fn line_count(&self) -> usize {
        self.text.len_lines()
    }

    pub fn line_start(&self, line: usize) -> usize {
        self.text.line_to_char(line.min(self.line_count()))
    }

    /// Length of a line in chars, without its newline.
    pub fn line_len(&self, line: usize) -> usize {
        let slice = self.text.line(line);
        let len = slice.len_chars();
        if len > 0 && slice.char(len - 1) == '\n' {
            len - 1
        } else {
            len
        }
    }

    pub fn line_end(&self, line: usize) -> usize {
        self.line_start(line) + self.line_len(line)
    }

    pub fn line_text(&self, line: usize) -> String {
        let start = self.line_start(line);
        self.text.slice(start..start + self.line_len(line)).to_string()
    }

    pub fn line_of(&self, offset: usize) -> usize {
        self.text.char_to_line(offset.min(self.len_chars()))
    }

    pub fn line_col(&self, offset: usize) -> (usize, usize) {
        let line = self.line_of(offset);
        (line, offset - self.line_start(line))
    }

    pub fn slice(&self, range: Range<usize>) -> String {
        self.text.slice(range).to_string()
    }

    pub fn char_at(&self, offset: usize) -> Option<char> {
        (offset < self.len_chars()).then(|| self.text.char(offset))
    }

    pub fn selections(&self) -> &[Selection] {
        &self.selections
    }

    pub fn newest_selection(&self) -> Selection {
        self.selections[self.newest]
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    // ---------------------------------------------------------------
    // display columns (tabs expanded, wide chars counted as 2)

    pub fn display_col(&self, line: usize, col: usize) -> usize {
        let start = self.line_start(line);
        let mut dcol = 0;
        for c in self.text.slice(start..start + col.min(self.line_len(line))).chars() {
            dcol = self.advance_col(dcol, c);
        }
        dcol
    }

    /// Char column on `line` closest to display column `dcol`.
    pub fn col_for_display(&self, line: usize, dcol: usize) -> usize {
        let start = self.line_start(line);
        let len = self.line_len(line);
        let mut cur = 0;
        for (i, c) in self.text.slice(start..start + len).chars().enumerate() {
            let next = self.advance_col(cur, c);
            if next > dcol {
                return if dcol - cur > (next - cur) / 2 { i + 1 } else { i };
            }
            cur = next;
        }
        len
    }

    fn advance_col(&self, col: usize, c: char) -> usize {
        if c == '\t' {
            col + self.tab_size - col % self.tab_size
        } else {
            col + c.width().unwrap_or(0)
        }
    }

    // ---------------------------------------------------------------
    // selection management

    pub fn set_cursor(&mut self, offset: usize) {
        let offset = offset.min(self.len_chars());
        self.set_selections(vec![Selection::cursor(offset)], 0);
    }

    pub fn set_selections(&mut self, selections: Vec<Selection>, newest: usize) {
        assert!(!selections.is_empty());
        self.selections = selections;
        self.newest = newest.min(self.selections.len() - 1);
        self.normalize();
        self.last_edit = None;
        self.whole_word_search = false;
    }

    /// Replace the newest selection, e.g. while dragging with the mouse.
    pub fn set_newest(&mut self, selection: Selection) {
        self.whole_word_search = false;
        self.selections[self.newest] = selection;
        self.normalize();
        self.last_edit = None;
    }

    pub fn add_selection(&mut self, selection: Selection) {
        self.selections.push(selection);
        self.newest = self.selections.len() - 1;
        self.normalize();
        self.last_edit = None;
    }

    /// Alt+click: add a cursor, or remove the one already there.
    pub fn toggle_cursor(&mut self, offset: usize) {
        if self.selections.len() > 1 {
            if let Some(i) = self
                .selections
                .iter()
                .position(|s| s.is_empty() && s.head == offset)
            {
                self.selections.remove(i);
                self.newest = self.selections.len() - 1;
                return;
            }
        }
        self.add_selection(Selection::cursor(offset));
    }

    /// Escape: back to a single cursor. Returns false if there was nothing to collapse.
    pub fn collapse_selections(&mut self) -> bool {
        if self.selections.len() > 1 {
            let s = self.newest_selection();
            self.set_selections(vec![s], 0);
            true
        } else if !self.selections[0].is_empty() {
            let head = self.selections[0].head;
            self.set_cursor(head);
            true
        } else {
            false
        }
    }

    fn normalize(&mut self) {
        let len = self.len_chars();
        let mut tagged: Vec<(Selection, bool)> = self
            .selections
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let mut s = *s;
                s.anchor = s.anchor.min(len);
                s.head = s.head.min(len);
                (s, i == self.newest)
            })
            .collect();
        tagged.sort_by_key(|(s, _)| (s.start(), s.end()));

        let mut merged: Vec<(Selection, bool)> = Vec::with_capacity(tagged.len());
        for (s, is_newest) in tagged {
            if let Some((prev, prev_newest)) = merged.last_mut() {
                let overlaps = s.start() < prev.end()
                    || s.start() == prev.start()
                    || (s.start() == prev.end() && (s.is_empty() || prev.is_empty()));
                if overlaps {
                    let start = prev.start().min(s.start());
                    let end = prev.end().max(s.end());
                    *prev = if prev.reversed() {
                        Selection::new(end, start)
                    } else {
                        Selection::new(start, end)
                    };
                    *prev_newest |= is_newest;
                    continue;
                }
            }
            merged.push((s, is_newest));
        }
        self.newest = merged.iter().position(|(_, n)| *n).unwrap_or(merged.len() - 1);
        self.selections = merged.into_iter().map(|(s, _)| s).collect();
    }

    // ---------------------------------------------------------------
    // core edit primitive

    /// Apply non-overlapping edits given in current coordinates. Selections are
    /// mapped through the edits. Returns the start offset of each edit after
    /// applying, in the same order as given.
    fn apply(&mut self, edits: Vec<(Range<usize>, String)>, kind: EditKind) -> Vec<usize> {
        let mut order: Vec<usize> = (0..edits.len()).collect();
        order.sort_by_key(|&i| edits[i].0.start);
        if edits.iter().all(|(r, t)| r.is_empty() && t.is_empty()) {
            return edits.iter().map(|(r, _)| r.start).collect();
        }

        let group = kind != EditKind::Other
            && self.last_edit == Some(kind)
            && !edits.iter().any(|(_, t)| t.contains('\n'));
        if !group {
            self.undo.push(Snapshot {
                text: self.text.clone(),
                selections: self.selections.clone(),
                newest: self.newest,
            });
            if self.undo.len() > UNDO_LIMIT {
                self.undo.remove(0);
            }
        }
        self.redo.clear();
        self.last_edit = Some(kind);

        let first_line = self.line_of(edits[order[0]].0.start);
        self.dirty_from_line = Some(self.dirty_from_line.map_or(first_line, |l| l.min(first_line)));

        for &i in order.iter().rev() {
            let (range, text) = &edits[i];
            if !range.is_empty() {
                self.text.remove(range.clone());
            }
            if !text.is_empty() {
                self.text.insert(range.start, text);
            }
        }

        let sorted: Vec<(Range<usize>, usize)> = order
            .iter()
            .map(|&i| (edits[i].0.clone(), edits[i].1.chars().count()))
            .collect();
        let map = |pos: usize| -> usize {
            let mut delta: isize = 0;
            for (range, new_len) in &sorted {
                if pos < range.start {
                    break;
                }
                if pos <= range.end {
                    return (range.start as isize + delta) as usize + new_len;
                }
                delta += *new_len as isize - range.len() as isize;
            }
            (pos as isize + delta) as usize
        };
        for s in &mut self.selections {
            s.anchor = map(s.anchor);
            s.head = map(s.head);
            s.goal = None;
        }

        let mut starts = vec![0; edits.len()];
        let mut delta: isize = 0;
        for &i in &order {
            starts[i] = (edits[i].0.start as isize + delta) as usize;
            delta += edits[i].1.chars().count() as isize - edits[i].0.len() as isize;
        }
        self.version += 1;
        self.normalize();
        starts
    }

    pub fn undo(&mut self) -> bool {
        let Some(snap) = self.undo.pop() else {
            return false;
        };
        self.redo.push(Snapshot {
            text: std::mem::replace(&mut self.text, snap.text),
            selections: std::mem::replace(&mut self.selections, snap.selections),
            newest: self.newest,
        });
        self.newest = snap.newest;
        self.after_history_jump();
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(snap) = self.redo.pop() else {
            return false;
        };
        self.undo.push(Snapshot {
            text: std::mem::replace(&mut self.text, snap.text),
            selections: std::mem::replace(&mut self.selections, snap.selections),
            newest: self.newest,
        });
        self.newest = snap.newest;
        self.after_history_jump();
        true
    }

    fn after_history_jump(&mut self) {
        self.last_edit = None;
        self.version += 1;
        self.dirty_from_line = Some(0);
        self.normalize();
    }

    /// Replace the whole buffer, keeping undo history.
    pub fn replace_all(&mut self, text: &str) {
        let len = self.len_chars();
        self.apply(vec![(0..len, text.to_string())], EditKind::Other);
        self.set_cursor(0);
    }

    // ---------------------------------------------------------------
    // typing

    /// Insert text at every cursor, replacing selections.
    pub fn insert(&mut self, text: &str) {
        let edits = self
            .selections
            .iter()
            .map(|s| (s.range(), text.to_string()))
            .collect();
        self.apply(edits, EditKind::Insert);
    }

    /// Replace one explicit range (IME composition). Cursor lands after the text.
    pub fn replace_range(&mut self, range: Range<usize>, text: &str) {
        let range = range.start.min(self.len_chars())..range.end.min(self.len_chars());
        let start = self.apply(vec![(range, text.to_string())], EditKind::Insert)[0];
        let at = start + text.chars().count();
        self.selections[self.newest] = Selection::cursor(at);
        self.normalize();
    }

    /// Typed input: handles bracket/quote pairing and typing over closers.
    pub fn type_text(&mut self, text: &str) {
        let mut chars = text.chars();
        let (Some(c), None) = (chars.next(), chars.next()) else {
            self.insert(text);
            return;
        };

        let all_empty = self.selections.iter().all(|s| s.is_empty());

        if all_empty
            && is_closing(c)
            && self.selections.iter().all(|s| self.char_at(s.head) == Some(c))
        {
            self.move_each(|_, s| Selection::cursor(s.head + 1));
            self.last_edit = Some(EditKind::Insert);
            return;
        }

        if let Some(close) = closing_pair(c) {
            if !all_empty && self.selections.iter().all(|s| !s.is_empty()) {
                let edits = self
                    .selections
                    .iter()
                    .map(|s| (s.range(), format!("{c}{}{close}", self.slice(s.range()))))
                    .collect::<Vec<_>>();
                let lens: Vec<usize> = self.selections.iter().map(|s| s.end() - s.start()).collect();
                let starts = self.apply(edits, EditKind::Other);
                for (i, s) in self.selections.iter_mut().enumerate() {
                    *s = Selection::new(starts[i] + 1, starts[i] + 1 + lens[i]);
                }
                return;
            }

            let quote = c == close;
            let can_pair = all_empty
                && self.selections.iter().all(|s| {
                    let next_ok = self
                        .char_at(s.head)
                        .is_none_or(|n| n.is_whitespace() || matches!(n, ')' | ']' | '}' | ',' | ';'));
                    let prev_ok = !quote
                        || s.head == 0
                        || !self.char_at(s.head - 1).is_some_and(is_word_char);
                    next_ok && prev_ok
                });
            if can_pair {
                let pair = format!("{c}{close}");
                let edits = self
                    .selections
                    .iter()
                    .map(|s| (s.range(), pair.clone()))
                    .collect();
                let starts = self.apply(edits, EditKind::Insert);
                for (i, s) in self.selections.iter_mut().enumerate() {
                    *s = Selection::cursor(starts[i] + 1);
                }
                return;
            }
        }

        self.insert(text);
    }

    /// Enter: newline keeping the current indentation.
    pub fn newline(&mut self) {
        let unit = " ".repeat(self.tab_size);
        let mut cursor_after = Vec::new();
        let edits: Vec<(Range<usize>, String)> = self
            .selections
            .iter()
            .map(|s| {
                let (line, _) = self.line_col(s.start());
                let indent: String = self
                    .line_text(line)
                    .chars()
                    .take_while(|c| *c == ' ' || *c == '\t')
                    .take(s.start() - self.line_start(line))
                    .collect();
                let between = s.is_empty()
                    && s.head > 0
                    && matches!(
                        (self.char_at(s.head - 1), self.char_at(s.head)),
                        (Some('('), Some(')')) | (Some('['), Some(']')) | (Some('{'), Some('}'))
                    );
                if between {
                    let first = format!("\n{indent}{unit}");
                    cursor_after.push(first.chars().count());
                    (s.range(), format!("{first}\n{indent}"))
                } else {
                    let text = format!("\n{indent}");
                    cursor_after.push(text.chars().count());
                    (s.range(), text)
                }
            })
            .collect();
        let starts = self.apply(edits, EditKind::Other);
        for (i, s) in self.selections.iter_mut().enumerate() {
            *s = Selection::cursor(starts[i] + cursor_after[i]);
        }
    }

    pub fn backspace(&mut self) {
        let edits: Vec<(Range<usize>, String)> = self
            .selections
            .iter()
            .map(|s| {
                if !s.is_empty() {
                    return (s.range(), String::new());
                }
                let at = s.head;
                if at == 0 {
                    return (0..0, String::new());
                }
                let (line, col) = self.line_col(at);
                let before = self.slice(self.line_start(line)..at);
                if col > 0 && before.chars().all(|c| c == ' ') {
                    let remove = match col % self.tab_size {
                        0 => self.tab_size,
                        n => n,
                    };
                    return (at - remove..at, String::new());
                }
                let prev = self.text.char(at - 1);
                if let (Some(close), Some(next)) = (closing_pair(prev), self.char_at(at)) {
                    if close == next {
                        return (at - 1..at + 1, String::new());
                    }
                }
                (at - 1..at, String::new())
            })
            .collect();
        self.apply_deletes(edits);
    }

    pub fn delete_forward(&mut self) {
        let edits = self
            .selections
            .iter()
            .map(|s| {
                if s.is_empty() {
                    (s.head..self.next_grapheme(s.head), String::new())
                } else {
                    (s.range(), String::new())
                }
            })
            .collect();
        self.apply_deletes(edits);
    }

    pub fn delete_word_left(&mut self) {
        self.delete_to(|ed, s| ed.word_left(s.head)..s.head);
    }

    pub fn delete_word_right(&mut self) {
        self.delete_to(|ed, s| s.head..ed.word_right(s.head));
    }

    pub fn delete_to_line_start(&mut self) {
        self.delete_to(|ed, s| {
            let (line, col) = ed.line_col(s.head);
            if col == 0 {
                s.head.saturating_sub(1)..s.head
            } else {
                ed.line_start(line)..s.head
            }
        });
    }

    fn delete_to(&mut self, f: impl Fn(&Self, &Selection) -> Range<usize>) {
        let edits = self
            .selections
            .iter()
            .map(|s| {
                if s.is_empty() {
                    (f(self, s), String::new())
                } else {
                    (s.range(), String::new())
                }
            })
            .collect();
        self.apply_deletes(edits);
    }

    // ranges from neighbouring cursors may overlap
    fn apply_deletes(&mut self, mut edits: Vec<(Range<usize>, String)>) {
        edits.sort_by_key(|(r, _)| r.start);
        let mut merged: Vec<(Range<usize>, String)> = Vec::new();
        for (r, t) in edits {
            if let Some((last, _)) = merged.last_mut() {
                if r.start < last.end || (r.start == last.end && r.is_empty()) {
                    last.end = last.end.max(r.end);
                    continue;
                }
            }
            merged.push((r, t));
        }
        self.apply(merged, EditKind::Delete);
    }

    // ---------------------------------------------------------------
    // motion

    fn move_each(&mut self, f: impl Fn(&Self, &Selection) -> Selection) {
        let moved: Vec<Selection> = self.selections.iter().map(|s| f(self, s)).collect();
        self.selections = moved;
        self.normalize();
        self.last_edit = None;
        self.whole_word_search = false;
    }

    pub fn move_cursors(&mut self, motion: Motion, extend: bool) {
        self.move_each(|ed, s| {
            if !extend && !s.is_empty() {
                match motion {
                    Motion::Left => return Selection::cursor(s.start()),
                    Motion::Right => return Selection::cursor(s.end()),
                    _ => {}
                }
            }
            let from = if !extend && !s.is_empty() {
                match motion {
                    Motion::Up | Motion::PageUp(_) => s.start(),
                    Motion::Down | Motion::PageDown(_) => s.end(),
                    _ => s.head,
                }
            } else {
                s.head
            };
            let (head, goal) = ed.motion_target(from, s.goal, motion);
            Selection {
                anchor: if extend { s.anchor } else { head },
                head,
                goal,
            }
        });
    }

    fn motion_target(&self, from: usize, goal: Option<usize>, motion: Motion) -> (usize, Option<usize>) {
        match motion {
            Motion::Left => (self.prev_grapheme(from), None),
            Motion::Right => (self.next_grapheme(from), None),
            Motion::WordLeft => (self.word_left(from), None),
            Motion::WordRight => (self.word_right(from), None),
            Motion::Up => self.vertical(from, goal, -1),
            Motion::Down => self.vertical(from, goal, 1),
            Motion::PageUp(n) => self.vertical(from, goal, -(n as isize)),
            Motion::PageDown(n) => self.vertical(from, goal, n as isize),
            Motion::LineStart => {
                let (line, col) = self.line_col(from);
                let indent = self
                    .line_text(line)
                    .chars()
                    .take_while(|c| c.is_whitespace())
                    .count();
                let target = if col == indent { 0 } else { indent };
                (self.line_start(line) + target, None)
            }
            Motion::LineEnd => (self.line_end(self.line_of(from)), None),
            Motion::DocStart => (0, None),
            Motion::DocEnd => (self.len_chars(), None),
        }
    }

    fn vertical(&self, from: usize, goal: Option<usize>, delta: isize) -> (usize, Option<usize>) {
        let (line, col) = self.line_col(from);
        let goal = goal.unwrap_or_else(|| self.display_col(line, col));
        let target = line as isize + delta;
        if target < 0 {
            return (0, Some(goal));
        }
        let target = target as usize;
        if target >= self.line_count() {
            return (self.len_chars(), Some(goal));
        }
        (self.line_start(target) + self.col_for_display(target, goal), Some(goal))
    }

    fn next_grapheme(&self, at: usize) -> usize {
        let len = self.len_chars();
        if at >= len {
            return len;
        }
        let (line, col) = self.line_col(at);
        if col >= self.line_len(line) {
            return at + 1;
        }
        let text = self.line_text(line);
        let mut chars = 0;
        for g in text.graphemes(true) {
            let next = chars + g.chars().count();
            if chars >= col {
                return self.line_start(line) + next;
            }
            chars = next;
        }
        self.line_end(line)
    }

    fn prev_grapheme(&self, at: usize) -> usize {
        if at == 0 {
            return 0;
        }
        let (line, col) = self.line_col(at);
        if col == 0 {
            return at - 1;
        }
        let text = self.line_text(line);
        let mut chars = 0;
        let mut last = 0;
        for g in text.graphemes(true) {
            if chars >= col {
                break;
            }
            last = chars;
            chars += g.chars().count();
        }
        self.line_start(line) + last
    }

    pub fn word_left(&self, at: usize) -> usize {
        let mut i = at;
        let mut chars = self.text.chars_at(at);
        // skip whitespace but stop at a line start
        while let Some(c) = chars.prev() {
            if c == '\n' {
                if i == at {
                    return at - 1;
                }
                return i;
            }
            if !c.is_whitespace() {
                chars.next();
                break;
            }
            i -= 1;
        }
        let Some(first) = chars.prev() else {
            return i;
        };
        let cls = class(first);
        i -= 1;
        while let Some(c) = chars.prev() {
            if class(c) != cls || c == '\n' {
                break;
            }
            i -= 1;
        }
        i
    }

    pub fn word_right(&self, at: usize) -> usize {
        let mut i = at;
        let mut chars = self.text.chars_at(at);
        let mut first = None;
        for c in chars.by_ref() {
            if c == '\n' {
                if i == at {
                    return at + 1;
                }
                return i;
            }
            if !c.is_whitespace() {
                first = Some(c);
                break;
            }
            i += 1;
        }
        let Some(first) = first else {
            return i;
        };
        let cls = class(first);
        i += 1;
        for c in chars {
            if class(c) != cls || c == '\n' {
                break;
            }
            i += 1;
        }
        i
    }

    /// The word touching `at`, if any.
    pub fn word_at(&self, at: usize) -> Option<Range<usize>> {
        let on_word = self.char_at(at).is_some_and(is_word_char);
        let after_word = at > 0 && self.char_at(at - 1).is_some_and(is_word_char);
        if !on_word && !after_word {
            return None;
        }
        let mut start = at;
        while start > 0 && is_word_char(self.text.char(start - 1)) {
            start -= 1;
        }
        let mut end = at;
        while self.char_at(end).is_some_and(is_word_char) {
            end += 1;
        }
        Some(start..end)
    }

    /// Range a double click selects: a word, a run of punctuation, or whitespace.
    pub fn token_range_at(&self, at: usize) -> Range<usize> {
        if let Some(r) = self.word_at(at) {
            return r;
        }
        let Some(c) = self.char_at(at).filter(|c| *c != '\n') else {
            return at..at;
        };
        let cls = class(c);
        let mut start = at;
        while start > 0 && self.text.char(start - 1) != '\n' && class(self.text.char(start - 1)) == cls {
            start -= 1;
        }
        let mut end = at;
        while self.char_at(end).is_some_and(|c| c != '\n' && class(c) == cls) {
            end += 1;
        }
        start..end
    }

    pub fn select_all(&mut self) {
        let len = self.len_chars();
        self.set_selections(vec![Selection::new(0, len)], 0);
    }

    /// Cmd+L: select whole lines, extending on repeat.
    pub fn select_line(&mut self) {
        self.move_each(|ed, s| {
            let first = ed.line_of(s.start());
            let next = ed.line_of(s.end()) + 1;
            let end = if next >= ed.line_count() {
                ed.len_chars()
            } else {
                ed.line_start(next)
            };
            Selection::new(ed.line_start(first), end)
        });
    }

    // ---------------------------------------------------------------
    // multi-cursor

    /// Alt+Cmd+Up/Down: add a cursor on the line above the topmost / below the bottommost.
    pub fn add_cursor_vertical(&mut self, down: bool) {
        let from = if down {
            *self.selections.iter().max_by_key(|s| s.head).unwrap()
        } else {
            *self.selections.iter().min_by_key(|s| s.head).unwrap()
        };
        let line = self.line_of(from.head);
        if (down && line + 1 >= self.line_count()) || (!down && line == 0) {
            return;
        }
        let (head, goal) = self.vertical(from.head, from.goal, if down { 1 } else { -1 });
        self.add_selection(Selection {
            anchor: head,
            head,
            goal,
        });
    }

    /// Box selection between two (line, display column) points.
    pub fn column_select(&mut self, anchor: (usize, usize), head: (usize, usize)) {
        let (l0, l1) = (anchor.0.min(head.0), anchor.0.max(head.0));
        let l1 = l1.min(self.line_count() - 1);
        let mut sels = Vec::new();
        for line in l0..=l1 {
            let start = self.line_start(line);
            let a = start + self.col_for_display(line, anchor.1);
            let h = start + self.col_for_display(line, head.1);
            // skip short lines when the box doesn't touch them, like VS Code
            if a == h && anchor.1 != head.1 && self.display_col(line, self.line_len(line)) < anchor.1.min(head.1) {
                continue;
            }
            sels.push(Selection {
                anchor: a,
                head: h,
                goal: Some(head.1),
            });
        }
        if sels.is_empty() {
            let line = head.0.min(self.line_count() - 1);
            let at = self.line_end(line);
            sels.push(Selection::cursor(at));
        }
        let newest = if head.0 >= anchor.0 { sels.len() - 1 } else { 0 };
        self.set_selections(sels, newest);
    }

    /// Shift+Alt+I: a cursor at the end of every line in each selection.
    pub fn split_selection_into_lines(&mut self) {
        let mut sels = Vec::new();
        for s in &self.selections {
            let (l0, l1) = (self.line_of(s.start()), self.line_of(s.end()));
            if l0 == l1 {
                sels.push(*s);
                continue;
            }
            for line in l0..=l1 {
                let start = if line == l0 { s.start() } else { self.line_start(line) };
                let end = if line == l1 { s.end() } else { self.line_end(line) };
                if line == l1 && end == start && line != l0 {
                    continue;
                }
                sels.push(Selection::new(start, end));
            }
        }
        let n = sels.len();
        self.set_selections(sels, n - 1);
    }

    fn search_needle(&mut self) -> Option<String> {
        let newest = self.newest_selection();
        if newest.is_empty() {
            let word = self.word_at(newest.head)?;
            self.whole_word_search = true;
            self.selections[self.newest] = Selection::new(word.start, word.end);
            self.normalize();
            return None;
        }
        Some(self.slice(newest.range()))
    }

    /// Cmd+D: select the word under the cursor, then add the next match.
    pub fn select_next_occurrence(&mut self) {
        let Some(needle) = self.search_needle() else {
            self.last_edit = None;
            return;
        };
        let matches = self.find_all(&needle, self.whole_word_search);
        if matches.is_empty() {
            return;
        }
        let after = self.newest_selection().end();
        let taken = |r: &Range<usize>| self.selections.iter().any(|s| s.start() == r.start && s.end() == r.end);
        let next = matches
            .iter()
            .filter(|r| r.start >= after)
            .chain(matches.iter())
            .find(|r| !taken(r))
            .cloned();
        if let Some(r) = next {
            self.add_selection(Selection::new(r.start, r.end));
        }
    }

    /// Cmd+Shift+L: select every match of the current selection or word.
    pub fn select_all_occurrences(&mut self) {
        let needle = match self.search_needle() {
            Some(n) => n,
            None => self.slice(self.newest_selection().range()),
        };
        if needle.is_empty() {
            return;
        }
        let current = self.newest_selection().start();
        let matches = self.find_all(&needle, self.whole_word_search);
        if matches.is_empty() {
            return;
        }
        let newest = matches.iter().position(|r| r.start == current).unwrap_or(0);
        let sels = matches.into_iter().map(|r| Selection::new(r.start, r.end)).collect();
        self.set_selections(sels, newest);
    }

    /// Case-insensitive search, in char offsets.
    pub fn find_all(&self, needle: &str, whole_word: bool) -> Vec<Range<usize>> {
        if needle.is_empty() {
            return Vec::new();
        }
        let hay = self.text.to_string().to_ascii_lowercase();
        let needle = needle.to_ascii_lowercase();
        let mut out = Vec::new();
        let mut from = 0;
        while let Some(pos) = hay[from..].find(&needle) {
            let b = from + pos;
            let e = b + needle.len();
            let ok = !whole_word
                || (hay[..b].chars().next_back().is_none_or(|c| !is_word_char(c))
                    && hay[e..].chars().next().is_none_or(|c| !is_word_char(c)));
            if ok {
                let start = self.text.byte_to_char(b);
                out.push(start..start + needle.chars().count());
            }
            from = e;
        }
        out
    }

    // ---------------------------------------------------------------
    // line operations

    /// Line spans covered by the selections, merged when adjacent.
    fn line_blocks(&self, merge_adjacent: bool) -> Vec<(usize, usize)> {
        let mut blocks: Vec<(usize, usize)> = Vec::new();
        for s in &self.selections {
            let first = self.line_of(s.start());
            let mut last = self.line_of(s.end());
            if last > first && self.line_col(s.end()).1 == 0 {
                last -= 1;
            }
            if let Some(prev) = blocks.last_mut() {
                let gap = if merge_adjacent { 1 } else { 0 };
                if first <= prev.1 + gap {
                    prev.1 = prev.1.max(last);
                    continue;
                }
            }
            blocks.push((first, last));
        }
        blocks
    }

    /// Cmd+/: toggle `-- ` line comments.
    pub fn toggle_comment(&mut self) {
        let lines: Vec<usize> = self
            .line_blocks(false)
            .into_iter()
            .flat_map(|(a, b)| a..=b)
            .collect();
        let non_blank: Vec<usize> = lines
            .iter()
            .copied()
            .filter(|&l| !self.line_text(l).trim().is_empty())
            .collect();
        if non_blank.is_empty() {
            return;
        }
        let commented = non_blank
            .iter()
            .all(|&l| self.line_text(l).trim_start().starts_with("--"));
        let edits = if commented {
            non_blank
                .iter()
                .map(|&l| {
                    let text = self.line_text(l);
                    let indent = text.chars().take_while(|c| c.is_whitespace()).count();
                    let rest: String = text.chars().skip(indent + 2).collect();
                    let n = if rest.starts_with(' ') { 3 } else { 2 };
                    let at = self.line_start(l) + indent;
                    (at..at + n, String::new())
                })
                .collect()
        } else {
            let indent = non_blank
                .iter()
                .map(|&l| self.line_text(l).chars().take_while(|c| c.is_whitespace()).count())
                .min()
                .unwrap_or(0);
            non_blank
                .iter()
                .map(|&l| {
                    let at = self.line_start(l) + indent;
                    (at..at, "-- ".to_string())
                })
                .collect()
        };
        self.apply(edits, EditKind::Other);
    }

    /// Tab: indent selected lines, or insert spaces up to the next tab stop.
    pub fn tab(&mut self) {
        let multiline = self
            .selections
            .iter()
            .any(|s| self.line_of(s.start()) != self.line_of(s.end()));
        if multiline {
            self.indent();
            return;
        }
        let edits = self
            .selections
            .iter()
            .map(|s| {
                let (line, col) = self.line_col(s.start());
                let dcol = self.display_col(line, col);
                (s.range(), " ".repeat(self.tab_size - dcol % self.tab_size))
            })
            .collect();
        self.apply(edits, EditKind::Insert);
    }

    pub fn indent(&mut self) {
        let unit = " ".repeat(self.tab_size);
        let edits = self
            .line_blocks(false)
            .into_iter()
            .flat_map(|(a, b)| a..=b)
            .filter(|&l| self.line_len(l) > 0)
            .map(|l| {
                let at = self.line_start(l);
                (at..at, unit.clone())
            })
            .collect();
        self.apply(edits, EditKind::Other);
    }

    pub fn outdent(&mut self) {
        let edits = self
            .line_blocks(false)
            .into_iter()
            .flat_map(|(a, b)| a..=b)
            .filter_map(|l| {
                let text = self.line_text(l);
                let n = if text.starts_with('\t') {
                    1
                } else {
                    text.chars().take(self.tab_size).take_while(|c| *c == ' ').count()
                };
                (n > 0).then(|| {
                    let at = self.line_start(l);
                    (at..at + n, String::new())
                })
            })
            .collect();
        self.apply(edits, EditKind::Other);
    }

    /// Cmd+Shift+K
    pub fn delete_lines(&mut self) {
        let blocks = self.line_blocks(true);
        let goals: Vec<usize> = blocks
            .iter()
            .map(|&(a, _)| {
                let s = self.selections.iter().find(|s| self.line_of(s.head) >= a).unwrap();
                let (l, c) = self.line_col(s.head);
                self.display_col(l, c)
            })
            .collect();
        let last_line = self.line_count() - 1;
        let edits: Vec<(Range<usize>, String)> = blocks
            .iter()
            .map(|&(a, b)| {
                if b < last_line {
                    (self.line_start(a)..self.line_start(b + 1), String::new())
                } else if a > 0 {
                    (self.line_end(a - 1)..self.len_chars(), String::new())
                } else {
                    (0..self.len_chars(), String::new())
                }
            })
            .collect();
        let starts = self.apply(edits, EditKind::Other);
        let sels = starts
            .iter()
            .zip(goals)
            .map(|(&at, goal)| {
                let line = self.line_of(at);
                let pos = self.line_start(line) + self.col_for_display(line, goal);
                Selection::cursor(pos)
            })
            .collect();
        self.set_selections(sels, usize::MAX);
    }

    /// Shift+Alt+Up/Down
    pub fn duplicate_lines(&mut self, down: bool) {
        let blocks = self.line_blocks(true);
        let lens: Vec<usize> = blocks
            .iter()
            .map(|&(a, b)| self.line_end(b) - self.line_start(a) + 1)
            .collect();
        let edits = blocks
            .iter()
            .map(|&(a, b)| {
                let body = self.slice(self.line_start(a)..self.line_end(b));
                let at = self.line_end(b);
                (at..at, format!("\n{body}"))
            })
            .collect();
        let sels = self
            .selections
            .iter()
            .map(|s| {
                let line = self.line_of(s.start());
                let bi = blocks.iter().position(|&(a, b)| line >= a && line <= b).unwrap_or(0);
                let shift = lens[..bi].iter().sum::<usize>() + if down { lens[bi] } else { 0 };
                Selection::new(s.anchor + shift, s.head + shift)
            })
            .collect();
        let newest = self.newest;
        self.apply(edits, EditKind::Other);
        self.set_selections(sels, newest);
    }

    /// Alt+Up/Down: move selected lines.
    pub fn move_lines(&mut self, down: bool) {
        let blocks = self.line_blocks(true);
        let last = self.line_count() - 1;
        if (down && blocks.last().unwrap().1 >= last) || (!down && blocks[0].0 == 0) {
            return;
        }
        let mut edits = Vec::new();
        let mut shifts = Vec::new();
        for &(a, b) in &blocks {
            let body = self.slice(self.line_start(a)..self.line_end(b));
            if down {
                let next = self.line_text(b + 1);
                edits.push((self.line_start(a)..self.line_end(b + 1), format!("{next}\n{body}")));
                shifts.push((a, b, next.chars().count() as isize + 1));
            } else {
                let prev = self.line_text(a - 1);
                edits.push((self.line_start(a - 1)..self.line_end(b), format!("{body}\n{prev}")));
                shifts.push((a, b, -(prev.chars().count() as isize + 1)));
            }
        }
        let before = self.selections.clone();
        let old_line_of: Vec<usize> = before.iter().map(|s| self.line_of(s.start())).collect();
        self.apply(edits, EditKind::Other);
        let sels = before
            .iter()
            .zip(old_line_of)
            .map(|(s, line)| {
                let d = shifts
                    .iter()
                    .find(|(a, b, _)| line >= *a && line <= *b)
                    .map_or(0, |x| x.2);
                Selection::new((s.anchor as isize + d) as usize, (s.head as isize + d) as usize)
            })
            .collect();
        self.set_selections(sels, self.newest);
    }

    /// Cmd+Enter / Cmd+Shift+Enter
    pub fn insert_line(&mut self, below: bool) {
        let mut indents = Vec::new();
        let edits = self
            .selections
            .iter()
            .map(|s| {
                let line = self.line_of(s.head);
                let indent: String = self
                    .line_text(line)
                    .chars()
                    .take_while(|c| *c == ' ' || *c == '\t')
                    .collect();
                indents.push(indent.chars().count());
                if below {
                    let at = self.line_end(line);
                    (at..at, format!("\n{indent}"))
                } else {
                    let at = self.line_start(line);
                    (at..at, format!("{indent}\n"))
                }
            })
            .collect::<Vec<_>>();
        let starts = self.apply(edits, EditKind::Other);
        let sels = starts
            .iter()
            .zip(indents)
            .map(|(&at, n)| Selection::cursor(if below { at + 1 + n } else { at + n }))
            .collect();
        self.set_selections(sels, usize::MAX);
    }

    // ---------------------------------------------------------------
    // clipboard

    /// Text for Cmd+C. The flag is true when whole lines were copied
    /// because every selection was empty.
    pub fn copy_text(&self) -> (String, bool) {
        if self.selections.iter().all(|s| s.is_empty()) {
            let mut lines: Vec<usize> = self.selections.iter().map(|s| self.line_of(s.head)).collect();
            lines.dedup();
            let text = lines
                .iter()
                .map(|&l| format!("{}\n", self.line_text(l)))
                .collect::<String>();
            (text, true)
        } else {
            let parts: Vec<String> = self.selections.iter().map(|s| self.slice(s.range())).collect();
            (parts.join("\n"), false)
        }
    }

    pub fn cut(&mut self) -> (String, bool) {
        let copied = self.copy_text();
        if copied.1 {
            self.delete_lines();
        } else {
            let edits = self.selections.iter().map(|s| (s.range(), String::new())).collect();
            self.apply(edits, EditKind::Other);
        }
        copied
    }

    pub fn paste(&mut self, text: &str, whole_lines: bool) {
        let n = self.selections.len();
        let parts: Vec<&str> = text.strip_suffix('\n').unwrap_or(text).split('\n').collect();
        if n > 1 && parts.len() == n {
            let edits = self
                .selections
                .iter()
                .zip(parts)
                .map(|(s, p)| (s.range(), p.to_string()))
                .collect();
            self.apply(edits, EditKind::Other);
            return;
        }
        if whole_lines && self.selections.iter().all(|s| s.is_empty()) {
            let edits = self
                .selections
                .iter()
                .map(|s| {
                    let at = self.line_start(self.line_of(s.head));
                    (at..at, text.to_string())
                })
                .collect::<Vec<_>>();
            let before = self.selections.clone();
            let len = text.chars().count();
            self.apply(edits, EditKind::Other);
            let sels = before
                .iter()
                .enumerate()
                .map(|(i, s)| Selection::cursor(s.head + len * (i + 1)))
                .collect();
            self.set_selections(sels, self.newest);
            return;
        }
        let edits = self.selections.iter().map(|s| (s.range(), text.to_string())).collect();
        self.apply(edits, EditKind::Other);
    }
}

#[cfg(test)]
mod tests;
