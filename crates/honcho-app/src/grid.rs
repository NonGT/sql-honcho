//! The result grid: a painted element so only visible cells cost anything,
//! with a sticky header and row numbers.

pub mod export;

use std::ops::Range;

use gpui::{
    actions, div, fill, point, prelude::*, px, relative, size, App, Bounds, ClipboardItem,
    ContentMask, Context, CursorStyle, Element, ElementId, Entity, FocusHandle, Focusable, Font,
    GlobalElementId, Hitbox, HitboxBehavior, Hsla, InspectorElementId, LayoutId, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, ScrollWheelEvent, ShapedLine,
    SharedString, Style, TextRun, Window,
};
use honcho_db::{Column, DriverKind, Row, Value};

use crate::editor_view::{self, EditorView};
use crate::theme::{self, Theme};

actions!(
    grid,
    [
        CellUp,
        CellDown,
        CellLeft,
        CellRight,
        ExtendUp,
        ExtendDown,
        ExtendLeft,
        ExtendRight,
        PageUpCells,
        PageDownCells,
        FirstRow,
        LastRow,
        SelectAllCells,
        CopyCells,
        OpenCell,
        CloseViewer,
    ]
);

const ROW_HEIGHT: Pixels = px(22.);
const HEADER_HEIGHT: Pixels = px(24.);
const PAD: Pixels = px(6.);
const DEFAULT_WIDTH: Pixels = px(150.);
const MIN_WIDTH: Pixels = px(40.);
const SORT_BUTTON: Pixels = px(18.);
const EDGE: Pixels = px(4.);
const BAR: Pixels = px(10.);
const MIN_THUMB: Pixels = px(24.);
/// Cells show at most this much; the viewer has the rest.
const SHOWN_CHARS: usize = 300;

/// A block of cells in display order, from where the selection started to
/// where it ends. Rows are display rows (after sorting).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CellRange {
    pub anchor: (usize, usize),
    pub head: (usize, usize),
}

impl CellRange {
    fn cell(row: usize, col: usize) -> Self {
        Self {
            anchor: (row, col),
            head: (row, col),
        }
    }

    pub fn rows(&self) -> Range<usize> {
        self.anchor.0.min(self.head.0)..self.anchor.0.max(self.head.0) + 1
    }

    pub fn cols(&self) -> Range<usize> {
        self.anchor.1.min(self.head.1)..self.anchor.1.max(self.head.1) + 1
    }
}

#[derive(Clone, Copy)]
enum Drag {
    None,
    Cells,
    Rows,
    Cols,
    Resize {
        col: usize,
        start_x: Pixels,
        start_width: Pixels,
    },
    /// Dragging a scrollbar thumb: pointer and scroll offset at the start.
    ScrollY(Pixels, Pixels),
    ScrollX(Pixels, Pixels),
}

/// A scrollbar: the track and the thumb's offset and length along it.
#[derive(Clone, Copy)]
struct Bar {
    track: Bounds<Pixels>,
    start: Pixels,
    len: Pixels,
    /// How far the content can scroll.
    max: Pixels,
}

impl Bar {
    fn new(
        track: Bounds<Pixels>,
        along: Pixels,
        view: Pixels,
        content: Pixels,
        scroll: Pixels,
    ) -> Option<Self> {
        if content <= view || along <= MIN_THUMB {
            return None;
        }
        let len = (along * (view / content)).max(MIN_THUMB);
        let max = content - view;
        Some(Self {
            track,
            start: (along - len) * (scroll / max),
            len,
            max,
        })
    }

    /// Scroll offset per pixel of thumb movement.
    fn ratio(&self, along: Pixels) -> f32 {
        self.max / (along - self.len)
    }
}

#[derive(Clone, Copy)]
enum Hit {
    VBar,
    HBar,
    Corner,
    Header { col: usize, edge: bool, sort: bool },
    RowNumber(usize),
    Cell(usize, usize),
    Outside,
}

#[derive(Clone, Copy)]
struct Metrics {
    bounds: Bounds<Pixels>,
    row_number_width: Pixels,
}

pub struct ResultGrid {
    focus: FocusHandle,
    kind: DriverKind,
    pub columns: Vec<Column>,
    pub rows: Vec<Row>,
    widths: Vec<Pixels>,
    /// Display row to row index while sorted.
    order: Option<Vec<usize>>,
    /// Column and ascending.
    pub sort: Option<(usize, bool)>,
    scroll: Point<Pixels>,
    pub selection: Option<CellRange>,
    drag: Drag,
    metrics: Option<Metrics>,
    /// The font cells were last drawn in, for fitting a column to them.
    font: Option<(Font, Pixels)>,
    viewer: Option<Entity<EditorView>>,
}

impl ResultGrid {
    pub fn new(kind: DriverKind, columns: Vec<Column>, cx: &mut Context<Self>) -> Self {
        Self {
            focus: cx.focus_handle(),
            kind,
            widths: vec![DEFAULT_WIDTH; columns.len()],
            columns,
            rows: Vec::new(),
            order: None,
            sort: None,
            scroll: Point::default(),
            selection: None,
            drag: Drag::None,
            metrics: None,
            font: None,
            viewer: None,
        }
    }

    pub fn append(&mut self, rows: Vec<Row>, cx: &mut Context<Self>) {
        let from = self.rows.len();
        self.rows.extend(rows);
        // new rows go at the end until the query finishes and the sort reruns
        if let Some(order) = &mut self.order {
            order.extend(from..self.rows.len());
        }
        cx.notify();
    }

    /// The query is done: put streamed rows in their sorted place.
    pub fn finish(&mut self, cx: &mut Context<Self>) {
        if self.sort.is_some() {
            self.resort();
            cx.notify();
        }
    }

    fn row_index(&self, display: usize) -> usize {
        match &self.order {
            Some(o) => o[display],
            None => display,
        }
    }

    fn row(&self, display: usize) -> &Row {
        &self.rows[self.row_index(display)]
    }

    /// Ascending, descending, then back to the server's order.
    pub fn toggle_sort(&mut self, col: usize, cx: &mut Context<Self>) {
        self.sort = match self.sort {
            Some((c, true)) if c == col => Some((col, false)),
            Some((c, false)) if c == col => None,
            _ => Some((col, true)),
        };
        self.resort();
        cx.notify();
    }

    fn resort(&mut self) {
        let Some((col, asc)) = self.sort else {
            self.order = None;
            return;
        };
        let rows = &self.rows;
        let mut order: Vec<usize> = (0..rows.len()).collect();
        // stable, so equal values keep the server's order
        order.sort_by(|&a, &b| {
            let o = export::compare(&rows[a][col], &rows[b][col]);
            if asc {
                o
            } else {
                o.reverse()
            }
        });
        self.order = Some(order);
    }

    /// How many rows a copy takes.
    pub fn picked_rows(&self) -> usize {
        match self.selection {
            Some(s) => s.rows().filter(|&r| r < self.rows.len()).count(),
            None => self.rows.len(),
        }
    }

    /// Selected cells, or everything when nothing is selected.
    fn picked(&self) -> (Vec<&Column>, Vec<Vec<&Value>>) {
        let (rows, cols) = match self.selection {
            Some(s) => (s.rows(), s.cols()),
            None => (0..self.rows.len(), 0..self.columns.len()),
        };
        let columns = self.columns[cols.clone()].iter().collect();
        let rows = rows
            .filter(|&r| r < self.rows.len())
            .map(|r| self.row(r)[cols.clone()].iter().collect())
            .collect();
        (columns, rows)
    }

    fn copy_as(&self, format: Format, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(self.export(format)));
    }

    pub fn export(&self, format: Format) -> String {
        let (columns, rows) = self.picked();
        match format {
            Format::Tsv => export::tsv(&rows),
            Format::Csv => export::csv(&columns, &rows),
            Format::Json => export::json(&columns, &rows),
            Format::Insert => export::inserts(self.kind, &columns, &rows),
        }
    }

    /// Everything, ignoring the selection, for saving to a file.
    pub fn export_all(&self, format: Format) -> String {
        let columns: Vec<&Column> = self.columns.iter().collect();
        let rows: Vec<Vec<&Value>> = (0..self.rows.len())
            .map(|r| self.row(r).iter().collect())
            .collect();
        match format {
            Format::Tsv => export::tsv(&rows),
            Format::Csv => export::csv(&columns, &rows),
            Format::Json => export::json(&columns, &rows),
            Format::Insert => export::inserts(self.kind, &columns, &rows),
        }
    }

    fn total_width(&self) -> Pixels {
        self.widths.iter().fold(px(0.), |a, w| a + *w)
    }

    fn col_left(&self, col: usize) -> Pixels {
        self.widths[..col].iter().fold(px(0.), |a, w| a + *w)
    }

    fn clamp_scroll(&mut self) {
        let Some(m) = self.metrics else {
            return;
        };
        let view_w = m.bounds.size.width - m.row_number_width;
        let view_h = m.bounds.size.height - HEADER_HEIGHT;
        let max_x = (self.total_width() - view_w).max(px(0.));
        let max_y = (ROW_HEIGHT * self.rows.len() as f32 - view_h).max(px(0.));
        self.scroll.x = self.scroll.x.max(px(0.)).min(max_x);
        self.scroll.y = self.scroll.y.max(px(0.)).min(max_y);
    }

    fn vbar(&self) -> Option<Bar> {
        let m = self.metrics?;
        let b = m.bounds;
        let track = Bounds::from_corners(
            point(b.right() - BAR, b.top() + HEADER_HEIGHT),
            b.bottom_right(),
        );
        Bar::new(
            track,
            track.size.height,
            b.size.height - HEADER_HEIGHT,
            ROW_HEIGHT * self.rows.len() as f32,
            self.scroll.y,
        )
    }

    fn hbar(&self) -> Option<Bar> {
        let m = self.metrics?;
        let b = m.bounds;
        let track = Bounds::from_corners(
            point(b.left() + m.row_number_width, b.bottom() - BAR),
            point(b.right() - BAR, b.bottom()),
        );
        Bar::new(
            track,
            track.size.width,
            b.size.width - m.row_number_width,
            self.total_width(),
            self.scroll.x,
        )
    }

    fn hit(&self, pos: Point<Pixels>) -> Hit {
        let Some(m) = self.metrics else {
            return Hit::Outside;
        };
        if self.vbar().is_some_and(|b| b.track.contains(&pos)) {
            return Hit::VBar;
        }
        if self.hbar().is_some_and(|b| b.track.contains(&pos)) {
            return Hit::HBar;
        }
        let local = pos - m.bounds.origin;
        if local.x < px(0.) || local.y < px(0.) {
            return Hit::Outside;
        }
        let in_header = local.y < HEADER_HEIGHT;
        if local.x < m.row_number_width {
            if in_header {
                return Hit::Corner;
            }
            let row = ((local.y - HEADER_HEIGHT + self.scroll.y) / ROW_HEIGHT).floor() as usize;
            return Hit::RowNumber(row.min(self.rows.len().saturating_sub(1)));
        }
        let x = local.x - m.row_number_width + self.scroll.x;
        let mut left = px(0.);
        let mut col = None;
        for (i, w) in self.widths.iter().enumerate() {
            if x < left + *w {
                col = Some((i, x - left, *w));
                break;
            }
            left += *w;
        }
        let Some((col, within, width)) = col else {
            // past the last column; its right edge still resizes
            let last = self.widths.len().saturating_sub(1);
            if in_header && !self.widths.is_empty() && x - left < EDGE {
                return Hit::Header {
                    col: last,
                    edge: true,
                    sort: false,
                };
            }
            return Hit::Outside;
        };
        if in_header {
            let edge = within > width - EDGE;
            // the left edge belongs to the column before
            if within < EDGE && col > 0 {
                return Hit::Header {
                    col: col - 1,
                    edge: true,
                    sort: false,
                };
            }
            let sort = !edge && within > width - EDGE - SORT_BUTTON;
            return Hit::Header { col, edge, sort };
        }
        let row = ((local.y - HEADER_HEIGHT + self.scroll.y) / ROW_HEIGHT).floor() as usize;
        if row >= self.rows.len() {
            return Hit::Outside;
        }
        Hit::Cell(row, col)
    }

    /// Widens or narrows a column to its header and longest value, as a
    /// double click on a header edge does.
    pub fn fit_column(&mut self, col: usize, window: &mut Window) {
        let Some((font, font_size)) = self.font.clone() else {
            return;
        };
        // shaping every row is slow on big results; in a monospace font the
        // longest texts are the widest ones
        let mut texts: Vec<String> = self.rows.iter().map(|r| cell_text(&r[col])).collect();
        texts.sort_by_key(|t| std::cmp::Reverse(t.chars().count()));
        texts.truncate(20);
        let text_system = window.text_system().clone();
        let width = |text: &str| {
            let run = TextRun {
                len: text.len(),
                font: font.clone(),
                color: Theme::text(),
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            text_system
                .shape_line(
                    SharedString::from(text.to_string()),
                    font_size,
                    &[run],
                    None,
                )
                .width
        };
        let header = width(&self.columns[col].name) + PAD + SORT_BUTTON;
        let cells = texts
            .iter()
            .map(|t| width(t) + PAD * 2.)
            .fold(px(0.), |a, w| a.max(w));
        // never past the view, so the column stays on screen whole
        let max = self
            .metrics
            .map(|m| m.bounds.size.width - m.row_number_width)
            .unwrap_or(px(800.))
            .max(MIN_WIDTH);
        self.widths[col] = header.max(cells).max(MIN_WIDTH).min(max);
    }

    #[cfg(test)]
    pub fn width(&self, col: usize) -> Pixels {
        self.widths[col]
    }

    /// Where the right edge of a column's header is on screen.
    #[cfg(test)]
    pub fn header_edge(&self, col: usize) -> Point<Pixels> {
        let m = self.metrics.expect("drawn");
        point(
            m.bounds.left() + m.row_number_width - self.scroll.x + self.col_left(col + 1),
            m.bounds.top() + HEADER_HEIGHT / 2.,
        )
    }

    fn last_row(&self) -> usize {
        self.rows.len().saturating_sub(1)
    }

    fn last_col(&self) -> usize {
        self.columns.len().saturating_sub(1)
    }

    fn on_mouse_down(&mut self, e: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus);
        let shift = e.modifiers.shift;
        let extend = |sel: &mut Option<CellRange>, head: (usize, usize)| match sel {
            Some(s) if shift => s.head = head,
            _ => *sel = Some(CellRange::cell(head.0, head.1)),
        };
        match self.hit(e.position) {
            Hit::VBar => {
                let bar = self.vbar().expect("hit a bar");
                let y = e.position.y - bar.track.top();
                // clicking off the thumb jumps there first
                if y < bar.start || y > bar.start + bar.len {
                    self.scroll.y = (y - bar.len / 2.) * bar.ratio(bar.track.size.height);
                    self.clamp_scroll();
                }
                self.drag = Drag::ScrollY(e.position.y, self.scroll.y);
            }
            Hit::HBar => {
                let bar = self.hbar().expect("hit a bar");
                let x = e.position.x - bar.track.left();
                if x < bar.start || x > bar.start + bar.len {
                    self.scroll.x = (x - bar.len / 2.) * bar.ratio(bar.track.size.width);
                    self.clamp_scroll();
                }
                self.drag = Drag::ScrollX(e.position.x, self.scroll.x);
            }
            Hit::Header {
                col, edge: true, ..
            } if e.click_count == 2 => {
                self.drag = Drag::None;
                self.fit_column(col, window);
            }
            Hit::Header {
                col, edge: true, ..
            } => {
                self.drag = Drag::Resize {
                    col,
                    start_x: e.position.x,
                    start_width: self.widths[col],
                };
            }
            Hit::Header {
                col, sort: true, ..
            } => self.toggle_sort(col, cx),
            Hit::Header { col, .. } if !self.rows.is_empty() => {
                let last_row = self.last_row();
                match &mut self.selection {
                    Some(s) if shift => s.head = (last_row, col),
                    sel => {
                        *sel = Some(CellRange {
                            anchor: (0, col),
                            head: (last_row, col),
                        })
                    }
                }
                self.drag = Drag::Cols;
            }
            Hit::RowNumber(row) if !self.rows.is_empty() => {
                let last_col = self.last_col();
                match &mut self.selection {
                    Some(s) if shift => s.head = (row, last_col),
                    sel => {
                        *sel = Some(CellRange {
                            anchor: (row, 0),
                            head: (row, last_col),
                        })
                    }
                }
                self.drag = Drag::Rows;
            }
            Hit::Corner => self.select_all(cx),
            Hit::Cell(row, col) => {
                if e.click_count == 2 {
                    self.selection = Some(CellRange::cell(row, col));
                    self.open_cell(window, cx);
                    return;
                }
                extend(&mut self.selection, (row, col));
                self.drag = Drag::Cells;
            }
            _ => {}
        }
        cx.notify();
    }

    fn on_mouse_move(&mut self, e: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if e.pressed_button != Some(MouseButton::Left) {
            return;
        }
        match self.drag {
            Drag::ScrollY(from, start) => {
                if let Some(bar) = self.vbar() {
                    self.scroll.y =
                        start + (e.position.y - from) * bar.ratio(bar.track.size.height);
                    self.clamp_scroll();
                    cx.notify();
                }
                return;
            }
            Drag::ScrollX(from, start) => {
                if let Some(bar) = self.hbar() {
                    self.scroll.x = start + (e.position.x - from) * bar.ratio(bar.track.size.width);
                    self.clamp_scroll();
                    cx.notify();
                }
                return;
            }
            _ => {}
        }
        let hit = self.hit(e.position);
        let Some(sel) = &mut self.selection else {
            if let Drag::Resize {
                col,
                start_x,
                start_width,
            } = self.drag
            {
                self.widths[col] = (start_width + e.position.x - start_x).max(MIN_WIDTH);
                cx.notify();
            }
            return;
        };
        match (self.drag, hit) {
            (
                Drag::Resize {
                    col,
                    start_x,
                    start_width,
                },
                _,
            ) => {
                self.widths[col] = (start_width + e.position.x - start_x).max(MIN_WIDTH);
            }
            (Drag::Cells, Hit::Cell(r, c)) => sel.head = (r, c),
            (Drag::Rows, Hit::RowNumber(r) | Hit::Cell(r, _)) => sel.head.0 = r,
            (Drag::Cols, Hit::Header { col, .. } | Hit::Cell(_, col)) => sel.head.1 = col,
            _ => return,
        }
        cx.notify();
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.drag = Drag::None;
    }

    fn on_scroll(&mut self, e: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let d = e.delta.pixel_delta(ROW_HEIGHT);
        // shift turns a mouse wheel sideways
        let d = if e.modifiers.shift && d.x == px(0.) {
            point(d.y, px(0.))
        } else {
            d
        };
        self.scroll.x -= d.x;
        self.scroll.y -= d.y;
        self.clamp_scroll();
        cx.notify();
    }

    pub fn select_all(&mut self, cx: &mut Context<Self>) {
        if !self.rows.is_empty() && !self.columns.is_empty() {
            self.selection = Some(CellRange {
                anchor: (0, 0),
                head: (self.last_row(), self.last_col()),
            });
        }
        cx.notify();
    }

    fn step(&mut self, dr: isize, dc: isize, extend: bool, cx: &mut Context<Self>) {
        if self.rows.is_empty() || self.columns.is_empty() {
            return;
        }
        let clamp =
            |v: usize, d: isize, max: usize| (v as isize + d).clamp(0, max as isize) as usize;
        let (last_row, last_col) = (self.last_row(), self.last_col());
        let sel = self.selection.get_or_insert(CellRange::cell(0, 0));
        let head = (
            clamp(sel.head.0, dr, last_row),
            clamp(sel.head.1, dc, last_col),
        );
        if extend {
            sel.head = head;
        } else {
            *sel = CellRange::cell(head.0, head.1);
        }
        self.reveal(head);
        cx.notify();
    }

    fn page_rows(&self) -> isize {
        self.metrics.map_or(20, |m| {
            ((m.bounds.size.height - HEADER_HEIGHT) / ROW_HEIGHT)
                .floor()
                .max(1.) as isize
                - 1
        })
    }

    /// Scrolls just enough to show the cell.
    fn reveal(&mut self, (row, col): (usize, usize)) {
        let Some(m) = self.metrics else {
            return;
        };
        let view_w = m.bounds.size.width - m.row_number_width;
        let view_h = m.bounds.size.height - HEADER_HEIGHT;
        let top = ROW_HEIGHT * row as f32;
        if top < self.scroll.y {
            self.scroll.y = top;
        } else if top + ROW_HEIGHT > self.scroll.y + view_h {
            self.scroll.y = top + ROW_HEIGHT - view_h;
        }
        let left = self.col_left(col);
        let right = left + self.widths[col];
        if left < self.scroll.x {
            self.scroll.x = left;
        } else if right > self.scroll.x + view_w {
            self.scroll.x = (right - view_w).min(left);
        }
        self.clamp_scroll();
    }

    /// Shows the selected cell's full value, JSON pretty printed.
    fn open_cell(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(sel) = self.selection else {
            return;
        };
        let (row, col) = sel.head;
        let Some(value) = self.rows.get(self.row_index(row)).map(|r| r[col].clone()) else {
            return;
        };
        let text = match &value {
            Value::Json(j) | Value::Text(j) => serde_json::from_str::<serde_json::Value>(j)
                .ok()
                .filter(|v| v.is_object() || v.is_array())
                .and_then(|v| serde_json::to_string_pretty(&v).ok())
                .unwrap_or_else(|| j.clone()),
            v => v.display(usize::MAX).into_owned(),
        };
        let viewer = cx.new(|cx| EditorView::new(&text, None, 0, cx));
        // after this click is done, or the grid takes focus back
        let handle = viewer.focus_handle(cx);
        window.defer(cx, move |window, _| window.focus(&handle));
        self.viewer = Some(viewer);
        cx.notify();
    }

    #[cfg(test)]
    pub fn viewer_text(&self, cx: &App) -> Option<String> {
        self.viewer.as_ref().map(|v| v.read(cx).text())
    }

    fn close_viewer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.viewer = None;
        window.focus(&self.focus);
        cx.notify();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Tsv,
    Csv,
    Json,
    Insert,
}

impl Format {
    pub fn label(self) -> &'static str {
        match self {
            Format::Tsv => "TSV",
            Format::Csv => "CSV",
            Format::Json => "JSON",
            Format::Insert => "INSERT",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Format::Tsv => "tsv",
            Format::Csv => "csv",
            Format::Json => "json",
            Format::Insert => "sql",
        }
    }
}

impl Focusable for ResultGrid {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for ResultGrid {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let viewer =
            self.viewer.clone().map(|v| {
                div()
                    .absolute()
                    .inset_4()
                    .flex()
                    .flex_col()
                    .bg(Theme::bg())
                    .border_1()
                    .border_color(Theme::border())
                    .rounded_md()
                    .shadow_lg()
                    .occlude()
                    // the viewer is an editor; keep its keys from reaching the tab
                    .on_action(|_: &crate::query::RunStatement, _, _| {})
                    .on_action(|_: &crate::query::RunAll, _, _| {})
                    .on_action(cx.listener(|this, _: &editor_view::Cancel, window, cx| {
                        this.close_viewer(window, cx)
                    }))
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .justify_between()
                            .items_center()
                            .h(px(24.))
                            .px_2()
                            .border_b_1()
                            .border_color(Theme::border())
                            .text_color(Theme::muted())
                            .child("Value (Esc closes)")
                            .child(
                                div()
                                    .id("close-viewer")
                                    .px_2()
                                    .cursor_pointer()
                                    .hover(|d| d.text_color(Theme::text()))
                                    .child("×")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.close_viewer(window, cx)
                                    })),
                            ),
                    )
                    .child(div().flex_1().min_h_0().child(v))
            });
        div()
            .key_context("ResultGrid")
            .track_focus(&self.focus)
            .size_full()
            .relative()
            .font_family(theme::font_family(cx))
            .text_size(px(12.))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .on_action(cx.listener(|this, _: &CellUp, _, cx| this.step(-1, 0, false, cx)))
            .on_action(cx.listener(|this, _: &CellDown, _, cx| this.step(1, 0, false, cx)))
            .on_action(cx.listener(|this, _: &CellLeft, _, cx| this.step(0, -1, false, cx)))
            .on_action(cx.listener(|this, _: &CellRight, _, cx| this.step(0, 1, false, cx)))
            .on_action(cx.listener(|this, _: &ExtendUp, _, cx| this.step(-1, 0, true, cx)))
            .on_action(cx.listener(|this, _: &ExtendDown, _, cx| this.step(1, 0, true, cx)))
            .on_action(cx.listener(|this, _: &ExtendLeft, _, cx| this.step(0, -1, true, cx)))
            .on_action(cx.listener(|this, _: &ExtendRight, _, cx| this.step(0, 1, true, cx)))
            .on_action(cx.listener(|this, _: &PageUpCells, _, cx| {
                let n = this.page_rows();
                this.step(-n, 0, false, cx)
            }))
            .on_action(cx.listener(|this, _: &PageDownCells, _, cx| {
                let n = this.page_rows();
                this.step(n, 0, false, cx)
            }))
            .on_action(
                cx.listener(|this, _: &FirstRow, _, cx| this.step(isize::MIN / 2, 0, false, cx)),
            )
            .on_action(
                cx.listener(|this, _: &LastRow, _, cx| this.step(isize::MAX / 2, 0, false, cx)),
            )
            .on_action(cx.listener(|this, _: &SelectAllCells, _, cx| this.select_all(cx)))
            .on_action(cx.listener(|this, _: &CopyCells, _, cx| this.copy_as(Format::Tsv, cx)))
            .on_action(cx.listener(|this, _: &OpenCell, window, cx| this.open_cell(window, cx)))
            .on_action(
                cx.listener(|this, _: &CloseViewer, window, cx| this.close_viewer(window, cx)),
            )
            .child(GridElement { grid: cx.entity() })
            .children(viewer)
    }
}

struct GridElement {
    grid: Entity<ResultGrid>,
}

struct Text {
    /// Top left of the line box; gpui centers the text in `height`.
    origin: Point<Pixels>,
    height: Pixels,
    clip: Bounds<Pixels>,
    line: ShapedLine,
}

/// A value as a cell shows it: one line, cut short.
fn cell_text(v: &Value) -> String {
    let mut text: String = v
        .display(SHOWN_CHARS)
        .chars()
        .take(SHOWN_CHARS)
        .map(|ch| if ch == '\n' || ch == '\r' { '⏎' } else { ch })
        .collect();
    if text.is_empty() {
        text.push(' ');
    }
    text
}

struct Prepaint {
    quads: Vec<(Bounds<Pixels>, Hsla)>,
    /// Painted over the cells: header and row number backgrounds.
    sticky_quads: Vec<(Bounds<Pixels>, Hsla)>,
    cells: Vec<Text>,
    sticky: Vec<Text>,
    bars: Vec<Bounds<Pixels>>,
    edges: Vec<Hitbox>,
}

impl IntoElement for GridElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for GridElement {
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
        let text_system = window.text_system().clone();
        let em = text_system
            .em_width(text_system.resolve_font(&font), font_size)
            .unwrap_or(font_size * 0.6);
        let shape = |text: String, color: Hsla| {
            let run = TextRun {
                len: text.len(),
                font: font.clone(),
                color,
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            text_system.shape_line(SharedString::from(text), font_size, &[run], None)
        };

        self.grid.update(cx, |grid, _| {
            let digits = grid.rows.len().max(1).to_string().len();
            let rn_w = em * (digits as f32 + 2.);
            grid.font = Some((font.clone(), font_size));
            grid.metrics = Some(Metrics {
                bounds,
                row_number_width: rn_w,
            });
            grid.clamp_scroll();

            let mut p = Prepaint {
                quads: Vec::new(),
                sticky_quads: Vec::new(),
                cells: Vec::new(),
                sticky: Vec::new(),
                bars: Vec::new(),
                edges: Vec::new(),
            };
            let body_top = bounds.top() + HEADER_HEIGHT;
            let body =
                Bounds::from_corners(point(bounds.left() + rn_w, body_top), bounds.bottom_right());
            let n = grid.rows.len();
            let first = (grid.scroll.y / ROW_HEIGHT).floor() as usize;
            let visible = ((body.size.height / ROW_HEIGHT).ceil() as usize) + 1;
            let rows = first.min(n)..(first + visible).min(n);
            let row_top = |r: usize| body_top + ROW_HEIGHT * r as f32 - grid.scroll.y;

            // columns on screen and their left edges
            let mut cols = Vec::new();
            let mut x = bounds.left() + rn_w - grid.scroll.x;
            for (c, w) in grid.widths.iter().enumerate() {
                if x + *w > body.left() && x < bounds.right() {
                    cols.push((c, x, *w));
                }
                x += *w;
            }
            let grid_right = x;

            // zebra rows and the selection
            for r in rows.clone() {
                if r % 2 == 1 {
                    p.quads.push((
                        Bounds::from_corners(
                            point(body.left(), row_top(r)),
                            point(grid_right.min(bounds.right()), row_top(r) + ROW_HEIGHT),
                        ),
                        Theme::grid_stripe(),
                    ));
                }
            }
            if let Some(sel) = grid.selection {
                let (sr, sc) = (sel.rows(), sel.cols());
                let top = row_top(sr.start).max(body_top);
                let bottom = row_top(sr.end).min(bounds.bottom());
                let left = (bounds.left() + rn_w - grid.scroll.x + grid.col_left(sc.start))
                    .max(body.left());
                let right = bounds.left() + rn_w - grid.scroll.x + grid.col_left(sc.end);
                if top < bottom && left < right {
                    p.quads.push((
                        Bounds::from_corners(point(left, top), point(right, bottom)),
                        Theme::selection(),
                    ));
                }
            }

            for r in rows.clone() {
                let row = grid.row(r);
                for &(c, x, w) in &cols {
                    let v = &row[c];
                    let text = cell_text(v);
                    let color = if v.is_null() {
                        Theme::muted()
                    } else {
                        Theme::text()
                    };
                    let line = shape(text, color);
                    let clip = Bounds::from_corners(
                        point(x.max(body.left()), row_top(r)),
                        point(x + w - PAD / 2., row_top(r) + ROW_HEIGHT),
                    );
                    let tx = if v.is_numeric() {
                        x + w - PAD - line.width
                    } else {
                        x + PAD
                    };
                    p.cells.push(Text {
                        origin: point(tx, row_top(r)),
                        height: ROW_HEIGHT,
                        clip,
                        line,
                    });
                }
            }

            // header
            p.sticky_quads.push((
                Bounds::new(bounds.origin, size(bounds.size.width, HEADER_HEIGHT)),
                Theme::panel(),
            ));
            p.sticky_quads.push((
                Bounds::new(
                    point(bounds.left(), body_top - px(1.)),
                    size(bounds.size.width, px(1.)),
                ),
                Theme::border(),
            ));
            for &(c, x, w) in &cols {
                let name = grid.columns[c].name.clone();
                let clip = Bounds::from_corners(
                    point(x.max(body.left()), bounds.top()),
                    point(x + w - SORT_BUTTON, body_top),
                );
                p.sticky.push(Text {
                    origin: point(x + PAD, bounds.top()),
                    height: HEADER_HEIGHT,
                    clip,
                    line: shape(name, Theme::text()),
                });
                let arrow = match grid.sort {
                    Some((sc, true)) if sc == c => ("▲", Theme::accent()),
                    Some((sc, false)) if sc == c => ("▼", Theme::accent()),
                    _ => ("↕", Theme::gutter_text()),
                };
                let arrow_x = x + w - SORT_BUTTON;
                if arrow_x + SORT_BUTTON > body.left() {
                    p.sticky.push(Text {
                        origin: point(arrow_x + px(3.), bounds.top()),
                        height: HEADER_HEIGHT,
                        clip: Bounds::from_corners(
                            point(arrow_x.max(body.left()), bounds.top()),
                            point(x + w, body_top),
                        ),
                        line: shape(arrow.0.into(), arrow.1),
                    });
                }
                // column separator, also the resize handle
                let edge_x = x + w;
                if edge_x > body.left() {
                    p.sticky_quads.push((
                        Bounds::new(
                            point(edge_x - px(1.), bounds.top()),
                            size(px(1.), HEADER_HEIGHT),
                        ),
                        Theme::border(),
                    ));
                    p.edges.push(window.insert_hitbox(
                        Bounds::new(
                            point(edge_x - EDGE, bounds.top()),
                            size(EDGE * 2., HEADER_HEIGHT),
                        ),
                        HitboxBehavior::Normal,
                    ));
                }
            }

            // row numbers
            p.sticky_quads.push((
                Bounds::new(
                    point(bounds.left(), body_top),
                    size(rn_w, bounds.size.height - HEADER_HEIGHT),
                ),
                Theme::panel(),
            ));
            p.sticky_quads.push((
                Bounds::new(bounds.origin, size(rn_w, HEADER_HEIGHT)),
                Theme::panel(),
            ));
            for bar in [grid.vbar(), grid.hbar()].into_iter().flatten() {
                let vertical = bar.track.size.height > bar.track.size.width;
                let thumb = if vertical {
                    Bounds::new(
                        point(bar.track.left() + px(2.), bar.track.top() + bar.start),
                        size(BAR - px(4.), bar.len),
                    )
                } else {
                    Bounds::new(
                        point(bar.track.left() + bar.start, bar.track.top() + px(2.)),
                        size(bar.len, BAR - px(4.)),
                    )
                };
                p.bars.push(thumb);
            }
            for r in rows {
                let label = shape((r + 1).to_string(), Theme::gutter_text());
                let x = bounds.left() + rn_w - PAD - label.width;
                p.sticky.push(Text {
                    origin: point(x, row_top(r)),
                    height: ROW_HEIGHT,
                    clip: Bounds::from_corners(
                        point(bounds.left(), row_top(r).max(body_top)),
                        point(bounds.left() + rn_w, row_top(r) + ROW_HEIGHT),
                    ),
                    line: label,
                });
            }
            p
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
        window.paint_quad(fill(bounds, Theme::bg()));
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            for (b, color) in &p.quads {
                window.paint_quad(fill(*b, *color));
            }
            for t in p.cells.iter() {
                window.with_content_mask(Some(ContentMask { bounds: t.clip }), |window| {
                    t.line.paint(t.origin, t.height, window, cx).ok();
                });
            }
            for (b, color) in &p.sticky_quads {
                window.paint_quad(fill(*b, *color));
            }
            for t in p.sticky.iter() {
                window.with_content_mask(Some(ContentMask { bounds: t.clip }), |window| {
                    t.line.paint(t.origin, t.height, window, cx).ok();
                });
            }
            for b in &p.bars {
                window.paint_quad(fill(*b, Theme::selection_match()).corner_radii(px(3.)));
            }
            for h in &p.edges {
                window.set_cursor_style(CursorStyle::ResizeColumn, h);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_are_inclusive_either_way() {
        let r = CellRange {
            anchor: (5, 3),
            head: (2, 1),
        };
        assert_eq!(r.rows(), 2..6);
        assert_eq!(r.cols(), 1..4);
    }
}
