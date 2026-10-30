use std::ops::Range;
use std::path::PathBuf;
use std::sync::Mutex;

use gpui::{
    actions, fill, point, px, relative, size, App, Bounds, ClipboardItem, ContentMask, Context,
    CursorStyle, Element, ElementId, ElementInputHandler, Entity, EntityInputHandler, FocusHandle,
    Focusable, GlobalElementId, Hsla, InspectorElementId, IntoElement, KeyBinding, LayoutId,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels, Point,
    Render, ScrollWheelEvent, ShapedLine, SharedString, Style, Styled, TextRun, UTF16Selection,
    Window, div, prelude::*,
};
use honcho_core::sql::{self, Highlighter};
use honcho_core::{Editor, LineEnding, Motion, Selection};

use crate::theme::{Theme, FONT_FAMILY, LINE_HEIGHT_RATIO};
use crate::FontSize;

actions!(
    editor,
    [
        MoveLeft,
        MoveRight,
        MoveUp,
        MoveDown,
        SelectLeft,
        SelectRight,
        SelectUp,
        SelectDown,
        MoveWordLeft,
        MoveWordRight,
        SelectWordLeft,
        SelectWordRight,
        MoveLineStart,
        MoveLineEnd,
        SelectLineStart,
        SelectLineEnd,
        MoveDocStart,
        MoveDocEnd,
        SelectDocStart,
        SelectDocEnd,
        PageUp,
        PageDown,
        SelectPageUp,
        SelectPageDown,
        Backspace,
        Delete,
        DeleteWordLeft,
        DeleteWordRight,
        DeleteToLineStart,
        Newline,
        Tab,
        Indent,
        Outdent,
        InsertLineBelow,
        InsertLineAbove,
        Undo,
        Redo,
        SelectAll,
        SelectLine,
        Copy,
        Cut,
        Paste,
        AddCursorAbove,
        AddCursorBelow,
        SelectNextOccurrence,
        SelectAllOccurrences,
        SplitSelectionIntoLines,
        Cancel,
        ToggleComment,
        MoveLinesUp,
        MoveLinesDown,
        CopyLinesUp,
        CopyLinesDown,
        DeleteLines,
    ]
);

const CONTEXT: &str = "Editor";

pub fn bind_keys(cx: &mut App) {
    let c = Some(CONTEXT);
    cx.bind_keys([
        KeyBinding::new("left", MoveLeft, c),
        KeyBinding::new("right", MoveRight, c),
        KeyBinding::new("up", MoveUp, c),
        KeyBinding::new("down", MoveDown, c),
        KeyBinding::new("shift-left", SelectLeft, c),
        KeyBinding::new("shift-right", SelectRight, c),
        KeyBinding::new("shift-up", SelectUp, c),
        KeyBinding::new("shift-down", SelectDown, c),
        KeyBinding::new("alt-left", MoveWordLeft, c),
        KeyBinding::new("alt-right", MoveWordRight, c),
        KeyBinding::new("alt-shift-left", SelectWordLeft, c),
        KeyBinding::new("alt-shift-right", SelectWordRight, c),
        KeyBinding::new("cmd-left", MoveLineStart, c),
        KeyBinding::new("home", MoveLineStart, c),
        KeyBinding::new("ctrl-a", MoveLineStart, c),
        KeyBinding::new("cmd-right", MoveLineEnd, c),
        KeyBinding::new("end", MoveLineEnd, c),
        KeyBinding::new("ctrl-e", MoveLineEnd, c),
        KeyBinding::new("cmd-shift-left", SelectLineStart, c),
        KeyBinding::new("shift-home", SelectLineStart, c),
        KeyBinding::new("cmd-shift-right", SelectLineEnd, c),
        KeyBinding::new("shift-end", SelectLineEnd, c),
        KeyBinding::new("cmd-up", MoveDocStart, c),
        KeyBinding::new("cmd-down", MoveDocEnd, c),
        KeyBinding::new("cmd-shift-up", SelectDocStart, c),
        KeyBinding::new("cmd-shift-down", SelectDocEnd, c),
        KeyBinding::new("pageup", PageUp, c),
        KeyBinding::new("pagedown", PageDown, c),
        KeyBinding::new("shift-pageup", SelectPageUp, c),
        KeyBinding::new("shift-pagedown", SelectPageDown, c),
        KeyBinding::new("backspace", Backspace, c),
        KeyBinding::new("shift-backspace", Backspace, c),
        KeyBinding::new("delete", Delete, c),
        KeyBinding::new("alt-backspace", DeleteWordLeft, c),
        KeyBinding::new("alt-delete", DeleteWordRight, c),
        KeyBinding::new("cmd-backspace", DeleteToLineStart, c),
        KeyBinding::new("enter", Newline, c),
        KeyBinding::new("shift-enter", Newline, c),
        KeyBinding::new("tab", Tab, c),
        KeyBinding::new("shift-tab", Outdent, c),
        KeyBinding::new("cmd-]", Indent, c),
        KeyBinding::new("cmd-[", Outdent, c),
        KeyBinding::new("cmd-enter", InsertLineBelow, c),
        KeyBinding::new("cmd-shift-enter", InsertLineAbove, c),
        KeyBinding::new("cmd-z", Undo, c),
        KeyBinding::new("cmd-shift-z", Redo, c),
        KeyBinding::new("cmd-a", SelectAll, c),
        KeyBinding::new("cmd-l", SelectLine, c),
        KeyBinding::new("cmd-c", Copy, c),
        KeyBinding::new("cmd-x", Cut, c),
        KeyBinding::new("cmd-v", Paste, c),
        KeyBinding::new("alt-cmd-up", AddCursorAbove, c),
        KeyBinding::new("alt-cmd-down", AddCursorBelow, c),
        KeyBinding::new("cmd-d", SelectNextOccurrence, c),
        KeyBinding::new("cmd-shift-l", SelectAllOccurrences, c),
        KeyBinding::new("alt-shift-i", SplitSelectionIntoLines, c),
        KeyBinding::new("escape", Cancel, c),
        KeyBinding::new("cmd-/", ToggleComment, c),
        KeyBinding::new("alt-up", MoveLinesUp, c),
        KeyBinding::new("alt-down", MoveLinesDown, c),
        KeyBinding::new("alt-shift-up", CopyLinesUp, c),
        KeyBinding::new("alt-shift-down", CopyLinesDown, c),
        KeyBinding::new("cmd-shift-k", DeleteLines, c),
    ]);
}

/// Last text we put on the clipboard as whole lines, so pasting it inserts
/// above the cursor line like VS Code.
static LINE_CLIPBOARD: Mutex<Option<String>> = Mutex::new(None);

#[derive(Clone, Debug)]
enum Drag {
    None,
    Chars(usize),
    Words(Range<usize>),
    Lines(usize),
    Column((usize, usize)),
}

#[derive(Clone)]
struct LineLayout {
    line: usize,
    shaped: ShapedLine,
    /// Display byte offset for each char column (tabs expanded).
    col_bytes: Vec<usize>,
}

impl LineLayout {
    fn x_for_col(&self, col: usize) -> Pixels {
        let b = self.col_bytes[col.min(self.col_bytes.len() - 1)];
        self.shaped.x_for_index(b)
    }

    fn col_for_x(&self, x: Pixels) -> usize {
        let b = self.shaped.closest_index_for_x(x);
        let i = self.col_bytes.partition_point(|&cb| cb < b);
        if i < self.col_bytes.len() && self.col_bytes[i] == b {
            return i;
        }
        // inside an expanded tab: pick the nearer edge
        let i = i.min(self.col_bytes.len() - 1);
        if i == 0 {
            return 0;
        }
        let (a, z) = (self.x_for_col(i - 1), self.x_for_col(i));
        if x - a < z - x { i - 1 } else { i }
    }
}

#[derive(Clone)]
struct Layout {
    bounds: Bounds<Pixels>,
    /// Window position of column 0 of line 0, after scrolling.
    origin: Point<Pixels>,
    gutter_width: Pixels,
    line_height: Pixels,
    em_width: Pixels,
    lines: Vec<LineLayout>,
}

impl Layout {
    fn visible_rows(&self) -> usize {
        ((self.bounds.size.height / self.line_height).floor() as usize).max(1)
    }

    fn line(&self, line: usize) -> Option<&LineLayout> {
        let first = self.lines.first()?.line;
        self.lines.get(line.checked_sub(first)?)
    }
}

pub struct EditorView {
    focus_handle: FocusHandle,
    pub editor: Editor,
    highlighter: Highlighter,
    pub path: Option<PathBuf>,
    pub untitled_index: usize,
    pub line_ending: LineEnding,
    saved_version: u64,
    scroll: Point<Pixels>,
    autoscroll: bool,
    drag: Drag,
    marked_range: Option<Range<usize>>,
    layout: Option<Layout>,
}

impl EditorView {
    pub fn new(text: &str, path: Option<PathBuf>, untitled_index: usize, cx: &mut Context<Self>) -> Self {
        let (text, line_ending) = honcho_core::normalize_line_endings(text);
        let editor = Editor::new(&text);
        Self {
            focus_handle: cx.focus_handle(),
            saved_version: editor.version(),
            editor,
            highlighter: Highlighter::new(),
            path,
            untitled_index,
            line_ending,
            scroll: Point::default(),
            autoscroll: false,
            drag: Drag::None,
            marked_range: None,
            layout: None,
        }
    }

    pub fn title(&self) -> String {
        match &self.path {
            Some(p) => p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.display().to_string()),
            None => format!("Untitled-{}", self.untitled_index),
        }
    }

    pub fn is_dirty(&self) -> bool {
        self.editor.version() != self.saved_version
    }

    /// Untitled and never touched, so opening a file can reuse the tab.
    pub fn is_pristine(&self) -> bool {
        self.path.is_none() && self.editor.len_chars() == 0 && !self.editor.can_undo()
    }

    pub fn contents_for_save(&self) -> String {
        honcho_core::restore_line_endings(&self.editor.text(), self.line_ending)
    }

    pub fn mark_saved(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.path = Some(path);
        self.saved_version = self.editor.version();
        cx.notify();
    }

    fn edit(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut Editor)) {
        f(&mut self.editor);
        self.marked_range = None;
        self.autoscroll = true;
        cx.notify();
    }

    fn page_rows(&self) -> usize {
        self.layout.as_ref().map_or(30, |l| l.visible_rows().saturating_sub(1).max(1))
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        let (text, lines) = self.editor.copy_text();
        *LINE_CLIPBOARD.lock().unwrap() = lines.then(|| text.clone());
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }

    fn cut(&mut self, _: &Cut, _: &mut Window, cx: &mut Context<Self>) {
        let mut copied = (String::new(), false);
        self.edit(cx, |e| copied = e.cut());
        *LINE_CLIPBOARD.lock().unwrap() = copied.1.then(|| copied.0.clone());
        cx.write_to_clipboard(ClipboardItem::new_string(copied.0));
    }

    fn paste(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        let (text, _) = honcho_core::normalize_line_endings(&text);
        let lines = LINE_CLIPBOARD.lock().unwrap().as_deref() == Some(text.as_str());
        self.edit(cx, |e| e.paste(&text, lines));
    }

    fn cancel(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        if self.editor.collapse_selections() {
            cx.notify();
        } else {
            cx.propagate();
        }
    }

    fn page(&mut self, up: bool, extend: bool, cx: &mut Context<Self>) {
        let rows = self.page_rows();
        let lh = self.layout.as_ref().map_or(px(20.), |l| l.line_height);
        let motion = if up { Motion::PageUp(rows) } else { Motion::PageDown(rows) };
        self.edit(cx, |e| e.move_cursors(motion, extend));
        let delta = lh * rows as f32;
        self.scroll.y = if up { self.scroll.y - delta } else { self.scroll.y + delta };
        self.clamp_scroll();
    }

    fn clamp_scroll(&mut self) {
        let lh = self.layout.as_ref().map_or(px(20.), |l| l.line_height);
        let max_y = lh * (self.editor.line_count().saturating_sub(1)) as f32;
        self.scroll.y = self.scroll.y.max(px(0.)).min(max_y);
        self.scroll.x = self.scroll.x.max(px(0.));
    }

    // ---------------------------------------------------------------
    // mouse

    fn offset_for_position(&self, pos: Point<Pixels>) -> usize {
        let Some(layout) = &self.layout else {
            return 0;
        };
        let line_count = self.editor.line_count();
        let row = ((pos.y - layout.origin.y) / layout.line_height).floor();
        if row < 0. {
            return 0;
        }
        let line = row as usize;
        if line >= line_count {
            return self.editor.len_chars();
        }
        let x = (pos.x - layout.origin.x).max(px(0.));
        let col = match layout.line(line) {
            Some(l) => l.col_for_x(x),
            None => self
                .editor
                .col_for_display(line, (x / layout.em_width).round() as usize),
        };
        self.editor.line_start(line) + col
    }

    fn display_point(&self, pos: Point<Pixels>) -> (usize, usize) {
        let Some(layout) = &self.layout else {
            return (0, 0);
        };
        let row = ((pos.y - layout.origin.y) / layout.line_height).floor().max(0.) as usize;
        let line = row.min(self.editor.line_count() - 1);
        let col = ((pos.x - layout.origin.x) / layout.em_width).round().max(0.) as usize;
        (line, col)
    }

    fn select_lines(&mut self, from: usize, to: usize) {
        let (a, b) = (from.min(to), from.max(to));
        let end = if b + 1 >= self.editor.line_count() {
            self.editor.len_chars()
        } else {
            self.editor.line_start(b + 1)
        };
        let start = self.editor.line_start(a);
        let sel = if to < from {
            Selection::new(end, start)
        } else {
            Selection::new(start, end)
        };
        self.editor.set_selections(vec![sel], 0);
    }

    fn on_mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus_handle);
        let Some(layout) = &self.layout else {
            return;
        };
        let in_gutter = event.position.x < layout.bounds.left() + layout.gutter_width;
        let offset = self.offset_for_position(event.position);
        let m = event.modifiers;
        self.marked_range = None;

        if in_gutter {
            let line = self.editor.line_of(offset);
            self.select_lines(line, line);
            self.drag = Drag::Lines(line);
        } else if m.alt && m.shift {
            let p = self.display_point(event.position);
            self.editor.column_select(p, p);
            self.drag = Drag::Column(p);
        } else if m.alt {
            self.editor.toggle_cursor(offset);
            self.drag = Drag::Chars(offset);
        } else if m.shift {
            let anchor = self.editor.newest_selection().anchor;
            self.editor.set_newest(Selection::new(anchor, offset));
            self.drag = Drag::Chars(anchor);
        } else if event.click_count == 2 {
            let r = self.editor.token_range_at(offset);
            self.editor.set_selections(vec![Selection::new(r.start, r.end)], 0);
            self.drag = Drag::Words(r);
        } else if event.click_count >= 3 {
            let line = self.editor.line_of(offset);
            self.select_lines(line, line);
            self.drag = Drag::Lines(line);
        } else {
            self.editor.set_cursor(offset);
            self.drag = Drag::Chars(offset);
        }
        cx.notify();
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if event.pressed_button != Some(MouseButton::Left) {
            return;
        }
        let offset = self.offset_for_position(event.position);
        match self.drag.clone() {
            Drag::None => return,
            Drag::Chars(anchor) => self.editor.set_newest(Selection::new(anchor, offset)),
            Drag::Words(anchor) => {
                let r = self.editor.token_range_at(offset);
                let sel = if offset < anchor.start {
                    Selection::new(anchor.end, r.start)
                } else {
                    Selection::new(anchor.start, r.end.max(anchor.end))
                };
                self.editor.set_newest(sel);
            }
            Drag::Lines(from) => {
                let to = self.editor.line_of(offset);
                self.select_lines(from, to);
            }
            Drag::Column(anchor) => {
                let p = self.display_point(event.position);
                self.editor.column_select(anchor, p);
            }
        }
        self.autoscroll = true;
        cx.notify();
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.drag = Drag::None;
    }

    fn on_scroll(&mut self, event: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let lh = self.layout.as_ref().map_or(px(20.), |l| l.line_height);
        let d = event.delta.pixel_delta(lh);
        self.scroll.y -= d.y;
        self.scroll.x -= d.x;
        self.clamp_scroll();
        self.autoscroll = false;
        cx.notify();
    }

    // ---------------------------------------------------------------
    // utf-16 helpers for the platform input handler

    fn to_utf16(&self, offset: usize) -> usize {
        self.editor.rope().char_to_utf16_cu(offset.min(self.editor.len_chars()))
    }

    fn char_from_utf16(&self, offset: usize) -> usize {
        let rope = self.editor.rope();
        rope.utf16_cu_to_char(offset.min(rope.len_utf16_cu()))
    }

    fn range_from_utf16(&self, r: &Range<usize>) -> Range<usize> {
        self.char_from_utf16(r.start)..self.char_from_utf16(r.end)
    }
}

macro_rules! on_editor {
    ($el:expr, $cx:expr, { $($action:ident => $f:expr),* $(,)? }) => {{
        let el = $el;
        $(
            let el = el.on_action($cx.listener(|this: &mut EditorView, _: &$action, _: &mut Window, cx: &mut Context<EditorView>| {
                let f: fn(&mut Editor) = $f;
                this.edit(cx, f);
            }));
        )*
        el
    }};
}

impl Render for EditorView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let font_size = cx.global::<FontSize>().0;
        let el = div()
            .id("editor")
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(Theme::bg())
            .cursor(CursorStyle::IBeam)
            .font_family(FONT_FAMILY)
            .text_size(font_size)
            .line_height(font_size * LINE_HEIGHT_RATIO)
            .text_color(Theme::text())
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(|this, _: &PageUp, _, cx| this.page(true, false, cx)))
            .on_action(cx.listener(|this, _: &PageDown, _, cx| this.page(false, false, cx)))
            .on_action(cx.listener(|this, _: &SelectPageUp, _, cx| this.page(true, true, cx)))
            .on_action(cx.listener(|this, _: &SelectPageDown, _, cx| this.page(false, true, cx)))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_scroll_wheel(cx.listener(Self::on_scroll));

        let el = on_editor!(el, cx, {
            MoveLeft => |e| e.move_cursors(Motion::Left, false),
            MoveRight => |e| e.move_cursors(Motion::Right, false),
            MoveUp => |e| e.move_cursors(Motion::Up, false),
            MoveDown => |e| e.move_cursors(Motion::Down, false),
            SelectLeft => |e| e.move_cursors(Motion::Left, true),
            SelectRight => |e| e.move_cursors(Motion::Right, true),
            SelectUp => |e| e.move_cursors(Motion::Up, true),
            SelectDown => |e| e.move_cursors(Motion::Down, true),
            MoveWordLeft => |e| e.move_cursors(Motion::WordLeft, false),
            MoveWordRight => |e| e.move_cursors(Motion::WordRight, false),
            SelectWordLeft => |e| e.move_cursors(Motion::WordLeft, true),
            SelectWordRight => |e| e.move_cursors(Motion::WordRight, true),
            MoveLineStart => |e| e.move_cursors(Motion::LineStart, false),
            MoveLineEnd => |e| e.move_cursors(Motion::LineEnd, false),
            SelectLineStart => |e| e.move_cursors(Motion::LineStart, true),
            SelectLineEnd => |e| e.move_cursors(Motion::LineEnd, true),
            MoveDocStart => |e| e.move_cursors(Motion::DocStart, false),
            MoveDocEnd => |e| e.move_cursors(Motion::DocEnd, false),
            SelectDocStart => |e| e.move_cursors(Motion::DocStart, true),
            SelectDocEnd => |e| e.move_cursors(Motion::DocEnd, true),
            Backspace => |e| e.backspace(),
            Delete => |e| e.delete_forward(),
            DeleteWordLeft => |e| e.delete_word_left(),
            DeleteWordRight => |e| e.delete_word_right(),
            DeleteToLineStart => |e| e.delete_to_line_start(),
            Newline => |e| e.newline(),
            Tab => |e| e.tab(),
            Indent => |e| e.indent(),
            Outdent => |e| e.outdent(),
            InsertLineBelow => |e| e.insert_line(true),
            InsertLineAbove => |e| e.insert_line(false),
            Undo => |e| { e.undo(); },
            Redo => |e| { e.redo(); },
            SelectAll => |e| e.select_all(),
            SelectLine => |e| e.select_line(),
            AddCursorAbove => |e| e.add_cursor_vertical(false),
            AddCursorBelow => |e| e.add_cursor_vertical(true),
            SelectNextOccurrence => |e| e.select_next_occurrence(),
            SelectAllOccurrences => |e| e.select_all_occurrences(),
            SplitSelectionIntoLines => |e| e.split_selection_into_lines(),
            ToggleComment => |e| e.toggle_comment(),
            MoveLinesUp => |e| e.move_lines(false),
            MoveLinesDown => |e| e.move_lines(true),
            CopyLinesUp => |e| e.duplicate_lines(false),
            CopyLinesDown => |e| e.duplicate_lines(true),
            DeleteLines => |e| e.delete_lines(),
        });

        el.child(EditorElement { view: cx.entity() })
    }
}

impl Focusable for EditorView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EntityInputHandler for EditorView {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual_range.replace(self.to_utf16(range.start)..self.to_utf16(range.end));
        Some(self.editor.slice(range))
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let s = self.editor.newest_selection();
        Some(UTF16Selection {
            range: self.to_utf16(s.start())..self.to_utf16(s.end()),
            reversed: s.reversed(),
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked_range
            .as_ref()
            .map(|r| self.to_utf16(r.start)..self.to_utf16(r.end))
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let explicit = range_utf16
            .map(|r| self.range_from_utf16(&r))
            .or(self.marked_range.take());
        match explicit {
            Some(range) => self.editor.replace_range(range, text),
            None => self.editor.type_text(text),
        }
        self.marked_range = None;
        self.autoscroll = true;
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .map(|r| self.range_from_utf16(&r))
            .or(self.marked_range.clone())
            .unwrap_or_else(|| self.editor.newest_selection().range());
        self.editor.replace_range(range.clone(), new_text);
        let len = new_text.chars().count();
        self.marked_range = (len > 0).then(|| range.start..range.start + len);
        if let Some(sel) = new_selected_range_utf16 {
            let to_chars = |u: usize| {
                let mut units = 0;
                new_text.chars().take_while(|c| {
                    units += c.len_utf16();
                    units <= u
                }).count()
            };
            self.editor.set_newest(Selection::new(
                range.start + to_chars(sel.start),
                range.start + to_chars(sel.end),
            ));
        }
        self.autoscroll = true;
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        _element_bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let layout = self.layout.as_ref()?;
        let start = self.char_from_utf16(range_utf16.start);
        let (line, col) = self.editor.line_col(start);
        let x = layout.line(line).map_or(px(0.), |l| l.x_for_col(col));
        Some(Bounds::new(
            point(layout.origin.x + x, layout.origin.y + layout.line_height * line as f32),
            size(layout.em_width, layout.line_height),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        Some(self.to_utf16(self.offset_for_position(point)))
    }
}

// -------------------------------------------------------------------
// painting

struct EditorElement {
    view: Entity<EditorView>,
}

struct Prepaint {
    layout: Layout,
    quads: Vec<(Bounds<Pixels>, Hsla)>,
    text_quads: Vec<(Bounds<Pixels>, Hsla)>,
    cursors: Vec<Bounds<Pixels>>,
    gutter: Vec<(Point<Pixels>, ShapedLine)>,
    scrollbar: Option<Bounds<Pixels>>,
}

impl IntoElement for EditorElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

const MAX_SHAPED_CHARS: usize = 4000;

impl Element for EditorElement {
    type RequestLayoutState = ();
    type PrepaintState = Prepaint;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Prepaint {
        let style = window.text_style();
        let font = style.font();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let line_height = window.line_height();
        let text_system = window.text_system().clone();
        let font_id = text_system.resolve_font(&font);
        let em_width = text_system.em_width(font_id, font_size).unwrap_or(font_size * 0.6);
        let focused = self.view.read(cx).focus_handle.is_focused(window);

        self.view.update(cx, |view, _| {
            if let Some(line) = view.editor.take_dirty_from_line() {
                view.highlighter.invalidate_from(line);
            }
            let ed = &view.editor;
            let line_count = ed.line_count();
            let digits = line_count.to_string().len().max(3);
            let gutter_width = em_width * (digits as f32 + 3.);
            let text_left = bounds.left() + gutter_width;
            let text_width = bounds.size.width - gutter_width;
            let height = bounds.size.height;

            let newest = ed.newest_selection();
            let (head_line, head_col) = ed.line_col(newest.head);
            if view.autoscroll {
                let y = line_height * head_line as f32;
                if y < view.scroll.y {
                    view.scroll.y = y;
                } else if y + line_height > view.scroll.y + height {
                    view.scroll.y = y + line_height - height;
                }
            }
            let max_y = line_height * line_count.saturating_sub(1) as f32;
            view.scroll.y = view.scroll.y.max(px(0.)).min(max_y);

            let first = (view.scroll.y / line_height).floor() as usize;
            let last = (first + (height / line_height).ceil() as usize + 1).min(line_count);

            let mut lines = Vec::with_capacity(last - first);
            for l in first..last {
                lines.push(shape_line(
                    view, l, &font, font_size, line_height, &text_system,
                ));
            }

            if view.autoscroll {
                if let Some(ll) = lines.get(head_line.wrapping_sub(first)) {
                    let x = ll.x_for_col(head_col);
                    let pad = em_width * 4.;
                    if x < view.scroll.x + pad {
                        view.scroll.x = (x - pad).max(px(0.));
                    } else if x + pad > view.scroll.x + text_width {
                        view.scroll.x = x + pad - text_width;
                    }
                }
                view.autoscroll = false;
            }

            let pad_left = em_width;
            let origin = point(
                text_left + pad_left - view.scroll.x,
                bounds.top() - view.scroll.y,
            );
            let layout = Layout {
                bounds,
                origin,
                gutter_width,
                line_height,
                em_width,
                lines,
            };

            let mut quads = Vec::new();
            let mut text_quads = Vec::new();
            let mut cursors = Vec::new();
            let ed = &view.editor;
            let sels = ed.selections();
            let row_y = |line: usize| origin.y + line_height * line as f32;

            // current line highlight
            let mut cursor_lines = Vec::new();
            for s in sels {
                let line = ed.line_of(s.head);
                if line >= first && line < last && cursor_lines.last() != Some(&line) {
                    cursor_lines.push(line);
                    if s.is_empty() {
                        quads.push((
                            Bounds::new(point(bounds.left(), row_y(line)), size(bounds.size.width, line_height)),
                            Theme::current_line(),
                        ));
                    }
                }
            }

            let vis_start = ed.line_start(first);
            let vis_end = if last >= line_count { ed.len_chars() } else { ed.line_start(last) };

            // other matches of a single short selection
            if sels.len() == 1 && !newest.is_empty() && newest.end() - newest.start() <= 200 {
                let needle = ed.slice(newest.range());
                if !needle.contains('\n') && !needle.trim().is_empty() {
                    let needle = needle.to_ascii_lowercase();
                    for ll in &layout.lines {
                        let text = ed.line_text(ll.line).to_ascii_lowercase();
                        let line_start = ed.line_start(ll.line);
                        for (b, _) in text.match_indices(&needle) {
                            let col = text[..b].chars().count();
                            if line_start + col == newest.start() {
                                continue;
                            }
                            let n = needle.chars().count();
                            let (x0, x1) = (ll.x_for_col(col), ll.x_for_col(col + n));
                            text_quads.push((
                                Bounds::from_corners(
                                    point(origin.x + x0, row_y(ll.line)),
                                    point(origin.x + x1, row_y(ll.line) + line_height),
                                ),
                                Theme::selection_match(),
                            ));
                        }
                    }
                }
            }

            let from = sels.partition_point(|s| s.end() < vis_start);
            for s in &sels[from..] {
                if s.start() > vis_end {
                    break;
                }
                if !s.is_empty() {
                    let l0 = ed.line_of(s.start()).max(first);
                    let l1 = ed.line_of(s.end()).min(last.saturating_sub(1));
                    for l in l0..=l1 {
                        let Some(ll) = layout.line(l) else { continue };
                        let ls = ed.line_start(l);
                        let len = ed.line_len(l);
                        let c0 = s.start().saturating_sub(ls).min(len);
                        let past_end = s.end() > ls + len;
                        let c1 = if past_end { len } else { s.end() - ls };
                        let x0 = ll.x_for_col(c0);
                        let mut x1 = ll.x_for_col(c1);
                        if past_end {
                            x1 += em_width * 0.6;
                        }
                        if x1 > x0 {
                            text_quads.push((
                                Bounds::from_corners(
                                    point(origin.x + x0, row_y(l)),
                                    point(origin.x + x1, row_y(l) + line_height),
                                ),
                                Theme::selection(),
                            ));
                        }
                    }
                }
                if focused {
                    let (l, c) = ed.line_col(s.head);
                    if let Some(ll) = layout.line(l) {
                        let x = ll.x_for_col(c);
                        cursors.push(Bounds::new(
                            point(origin.x + x, row_y(l)),
                            size(px(2.), line_height),
                        ));
                    }
                }
            }

            if let Some(m) = &view.marked_range {
                let (l, c0) = ed.line_col(m.start);
                let c1 = c0 + (m.end - m.start);
                if let Some(ll) = layout.line(l) {
                    let (x0, x1) = (ll.x_for_col(c0), ll.x_for_col(c1));
                    text_quads.push((
                        Bounds::from_corners(
                            point(origin.x + x0, row_y(l) + line_height - px(2.)),
                            point(origin.x + x1, row_y(l) + line_height - px(1.)),
                        ),
                        Theme::text(),
                    ));
                }
            }

            // gutter numbers, right aligned
            let mut gutter = Vec::new();
            for ll in &layout.lines {
                let label: SharedString = (ll.line + 1).to_string().into();
                let color = if cursor_lines.contains(&ll.line) {
                    Theme::gutter_active()
                } else {
                    Theme::gutter_text()
                };
                let run = TextRun {
                    len: label.len(),
                    font: font.clone(),
                    color,
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                };
                let shaped = text_system.shape_line(label, font_size, &[run], None);
                let x = bounds.left() + gutter_width - em_width * 1.5 - shaped.width;
                gutter.push((point(x, row_y(ll.line)), shaped));
            }

            let total = line_height * line_count as f32 + height - line_height;
            let scrollbar = (total > height).then(|| {
                let thumb = (height * (height / total)).max(px(24.));
                let y = (height - thumb) * (view.scroll.y / (total - height)).min(1.);
                Bounds::new(
                    point(bounds.right() - px(10.), bounds.top() + y),
                    size(px(8.), thumb),
                )
            });

            view.layout = Some(layout.clone());
            Prepaint {
                layout,
                quads,
                text_quads,
                cursors,
                gutter,
                scrollbar,
            }
        })
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        p: &mut Prepaint,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.view.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.view.clone()),
            cx,
        );

        let layout = &p.layout;
        for (b, color) in &p.quads {
            window.paint_quad(fill(*b, *color));
        }

        let text_area = Bounds::from_corners(
            point(bounds.left() + layout.gutter_width, bounds.top()),
            bounds.bottom_right(),
        );
        window.with_content_mask(Some(ContentMask { bounds: text_area }), |window| {
            for (b, color) in &p.text_quads {
                window.paint_quad(fill(*b, *color));
            }
            for ll in &layout.lines {
                let y = layout.origin.y + layout.line_height * ll.line as f32;
                ll.shaped
                    .paint(point(layout.origin.x, y), layout.line_height, window, cx)
                    .ok();
            }
            for c in &p.cursors {
                window.paint_quad(fill(*c, Theme::cursor()));
            }
        });

        window.paint_quad(fill(
            Bounds::new(bounds.origin, size(layout.gutter_width, bounds.size.height)),
            Theme::bg(),
        ));
        for (o, shaped) in &p.gutter {
            shaped.paint(*o, layout.line_height, window, cx).ok();
        }
        if let Some(b) = p.scrollbar {
            window.paint_quad(fill(b, Theme::selection_match()).corner_radii(px(4.)));
        }
    }
}

fn shape_line(
    view: &mut EditorView,
    line: usize,
    font: &gpui::Font,
    font_size: Pixels,
    _line_height: Pixels,
    text_system: &std::sync::Arc<gpui::WindowTextSystem>,
) -> LineLayout {
    let rope = view.editor.rope();
    let tokens = view.highlighter.line_tokens(rope, line);
    let text = sql::line_string(rope, line);
    let tab = view.editor.tab_size;

    let mut display = String::with_capacity(text.len());
    let mut col_bytes = Vec::with_capacity(text.len() + 1);
    let mut runs: Vec<TextRun> = Vec::new();
    let mut ti = 0;
    let mut dcol = 0;
    for (i, (b, ch)) in text.char_indices().enumerate() {
        if i >= MAX_SHAPED_CHARS {
            break;
        }
        col_bytes.push(display.len());
        while ti < tokens.len() && tokens[ti].range.end <= b {
            ti += 1;
        }
        let color = match tokens.get(ti) {
            Some(t) if t.range.start <= b => Theme::token(t.kind),
            _ => Theme::text(),
        };
        let before = display.len();
        if ch == '\t' {
            let n = tab - dcol % tab;
            display.extend(std::iter::repeat_n(' ', n));
            dcol += n;
        } else {
            display.push(ch);
            dcol += 1;
        }
        let added = display.len() - before;
        match runs.last_mut() {
            Some(r) if r.color == color => r.len += added,
            _ => runs.push(TextRun {
                len: added,
                font: font.clone(),
                color,
                background_color: None,
                underline: None,
                strikethrough: None,
            }),
        }
    }
    let total_cols = view.editor.line_len(line);
    while col_bytes.len() <= total_cols {
        col_bytes.push(display.len());
    }

    let shaped = text_system.shape_line(display.into(), font_size, &runs, None);
    LineLayout {
        line,
        shaped,
        col_bytes,
    }
}
