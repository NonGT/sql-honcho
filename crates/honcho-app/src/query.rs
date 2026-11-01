//! A tab's connection and the queries run from it.

use std::ops::Range;
use std::time::{Duration, Instant};

use gpui::{
    actions, div, prelude::*, px, uniform_list, App, Context, Entity, EventEmitter, FocusHandle,
    Focusable, MouseButton, PromptLevel, SharedString, Subscription, Task, Window,
};
use honcho_config::ColorTag;
use honcho_core::Editor;
use honcho_db::script;
use honcho_db::{Column, DbError, DriverKind, MessageLevel, QueryEvent, Row, SessionOptions};
use tokio_util::sync::CancellationToken;

use crate::connection_dialog::color_dot;
use crate::connections::{self, ActiveConnection, Connections};
use crate::editor_view::EditorView;
use crate::theme::{self, Theme};

actions!(query, [RunStatement, RunAll, CancelQuery]);

const PANEL_HEIGHT: f32 = 240.;
const CELL_WIDTH: f32 = 150.;
const CELL_BYTES: usize = 200;

/// What a run will execute.
#[derive(Clone, Debug, PartialEq)]
pub struct Target {
    pub text: String,
    /// In chars, for highlighting.
    pub range: Range<usize>,
    /// 0-based line of the buffer where `text` starts, to turn the server's
    /// line numbers into the editor's.
    pub first_line: u32,
}

/// The selection if there is one, otherwise the statement at the cursor,
/// or the whole buffer when `whole`.
pub fn target(ed: &Editor, kind: DriverKind, whole: bool) -> Result<Target, &'static str> {
    let nothing = "Nothing to run.";
    if whole {
        let text = ed.text();
        if text.trim().is_empty() {
            return Err(nothing);
        }
        return Ok(Target {
            range: 0..ed.len_chars(),
            text,
            first_line: 0,
        });
    }
    let sels = ed.selections();
    if sels.len() > 1 {
        return Err("Run needs a single cursor or selection.");
    }
    let sel = ed.newest_selection();
    if !sel.is_empty() {
        let text = ed.slice(sel.range());
        if text.trim().is_empty() {
            return Err(nothing);
        }
        return Ok(Target {
            text,
            range: sel.range(),
            first_line: ed.line_of(sel.start()) as u32,
        });
    }
    let text = ed.text();
    let rope = ed.rope();
    let at = rope.char_to_byte(sel.head);
    let statements = script::split(kind, &text);
    // the statement around the cursor, or the last one before it
    let statement = statements
        .iter()
        .find(|s| s.start <= at && at <= s.start + s.text.len())
        .or_else(|| {
            statements
                .iter()
                .rev()
                .find(|s| s.start + s.text.len() <= at)
        })
        .ok_or(nothing)?;
    let end = statement.start + statement.text.len();
    Ok(Target {
        text: statement.text.to_string(),
        range: rope.byte_to_char(statement.start)..rope.byte_to_char(end),
        first_line: statement.line - 1,
    })
}

pub enum TabEvent {
    /// Run was asked for without a connection.
    NeedConnection,
}

pub struct ResultSet {
    pub columns: Vec<Column>,
    pub rows: Vec<Row>,
}

pub struct OutputMessage {
    pub level: MessageLevel,
    pub text: String,
    /// 0-based buffer line to jump to.
    pub line: Option<u32>,
}

#[derive(Default)]
pub struct Output {
    pub sets: Vec<ResultSet>,
    pub messages: Vec<OutputMessage>,
    /// Which sub-tab shows: a result set, or the messages after them.
    pub shown: usize,
}

impl Output {
    fn message(&mut self, level: MessageLevel, text: impl Into<String>, line: Option<u32>) {
        self.messages.push(OutputMessage {
            level,
            text: text.into(),
            line,
        });
    }
}

struct Running {
    started: Instant,
    cancel: CancellationToken,
    _events: Task<()>,
    _ticker: Task<()>,
}

pub struct QueryTab {
    focus: FocusHandle,
    pub editor: Entity<EditorView>,
    pub connection: Option<ActiveConnection>,
    pub database: Option<String>,
    databases: Option<Vec<String>>,
    running: Option<Running>,
    pub output: Option<Output>,
    /// Why the connection is gone, when it is.
    pub lost: Option<String>,
    reconnecting: bool,
    /// The editor state the run target was worked out for.
    target_key: Option<(u64, usize, Vec<honcho_core::Selection>)>,
    _subs: Vec<Subscription>,
}

impl EventEmitter<TabEvent> for QueryTab {}

impl QueryTab {
    pub fn new(editor: Entity<EditorView>, cx: &mut Context<Self>) -> Self {
        let subs = vec![cx.observe(&editor, |this, _, cx| {
            this.update_target(cx);
            cx.notify();
        })];
        Self {
            focus: cx.focus_handle(),
            editor,
            connection: None,
            database: None,
            databases: None,
            running: None,
            output: None,
            lost: None,
            reconnecting: false,
            target_key: None,
            _subs: subs,
        }
    }

    pub fn is_running(&self) -> bool {
        self.running.is_some()
    }

    pub fn color(&self) -> Option<ColorTag> {
        self.connection.as_ref().and_then(|c| c.saved.color)
    }

    fn kind(&self) -> Option<DriverKind> {
        self.connection.as_ref().map(|c| c.saved.driver)
    }

    pub fn set_connection(&mut self, conn: ActiveConnection, cx: &mut Context<Self>) {
        if let Some(old) = self.connection.replace(conn) {
            close_session(old, cx);
        }
        self.lost = None;
        self.database = None;
        self.databases = None;
        self.target_key = None;
        self.update_target(cx);
        self.refresh_database(cx);
        cx.notify();
    }

    /// Closes the session; called when the tab goes away.
    pub fn disconnect(&mut self, cx: &mut App) {
        if let Some(r) = self.running.take() {
            r.cancel.cancel();
        }
        if let Some(old) = self.connection.take() {
            close_session(old, cx);
        }
    }

    fn refresh_database(&mut self, cx: &mut Context<Self>) {
        let Some(conn) = &self.connection else {
            return;
        };
        let session = conn.session.clone();
        cx.spawn(async move |this, cx| {
            if let Ok(db) = session.current_database().await {
                this.update(cx, |this, cx| {
                    this.database = db;
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    fn update_target(&mut self, cx: &mut Context<Self>) {
        let ed = &self.editor.read(cx).editor;
        let key = (ed.revision(), ed.len_chars(), ed.selections().to_vec());
        if self.target_key.as_ref() == Some(&key) {
            return;
        }
        // a selection already shows what runs; mark the statement otherwise
        let range = match self.kind() {
            Some(kind) if key.2.len() == 1 && key.2[0].is_empty() => {
                target(ed, kind, false).ok().map(|t| t.range)
            }
            _ => None,
        };
        self.target_key = Some(key);
        self.editor.update(cx, |e, cx| {
            if e.run_target != range {
                e.run_target = range;
                cx.notify();
            }
        });
    }

    fn run_action(&mut self, whole: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(conn) = self.connection.clone() else {
            cx.emit(TabEvent::NeedConnection);
            return;
        };
        if self.running.is_some() {
            return;
        }
        let target = match target(&self.editor.read(cx).editor, conn.saved.driver, whole) {
            Ok(t) => t,
            Err(e) => {
                self.output = Some(Output::default());
                self.say(MessageLevel::Warning, e, None);
                return;
            }
        };
        let unfiltered = (conn.saved.color == Some(ColorTag::Red))
            .then(|| {
                script::split(conn.saved.driver, &target.text)
                    .iter()
                    .find_map(|s| script::unfiltered_write(s.text))
            })
            .flatten();
        let Some(kind) = unfiltered else {
            self.run(target, cx);
            return;
        };
        let msg = format!("{kind} without WHERE on {}?", conn.saved.name);
        let answer = window.prompt(
            PromptLevel::Warning,
            &msg,
            Some("It changes every row in the table."),
            // Cancel first so Enter doesn't confirm by reflex
            &["Cancel", "Run"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await == Ok(1) {
                this.update(cx, |this, cx| this.run(target, cx)).ok();
            }
        })
        .detach();
    }

    fn say(&mut self, level: MessageLevel, text: &str, line: Option<u32>) {
        let out = self.output.get_or_insert_with(Output::default);
        out.message(level, text, line);
        out.shown = out.sets.len();
    }

    pub fn run(&mut self, target: Target, cx: &mut Context<Self>) {
        let Some(conn) = &self.connection else {
            return;
        };
        if self.lost.is_some() {
            self.output = Some(Output::default());
            self.say(
                MessageLevel::Error,
                "The connection was lost. Reconnect first.",
                None,
            );
            cx.notify();
            return;
        }
        let mut handle = conn.session.execute(target.text);
        let cancel = handle.cancel_token();
        let first_line = target.first_line;
        self.output = Some(Output::default());

        let events = cx.spawn(async move |this, cx| {
            while let Some(event) = handle.next().await {
                let done = matches!(event, QueryEvent::Finished(_));
                let alive = this
                    .update(cx, |this, cx| {
                        this.apply(event, first_line);
                        cx.notify();
                    })
                    .is_ok();
                if done || !alive {
                    break;
                }
            }
            this.update(cx, |this, cx| {
                this.running = None;
                this.refresh_database(cx);
                cx.notify();
            })
            .ok();
        });
        // keeps the elapsed time moving
        let ticker = cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(100))
                .await;
            if this.update(cx, |_, cx| cx.notify()).is_err() {
                break;
            }
        });
        self.running = Some(Running {
            started: Instant::now(),
            cancel,
            _events: events,
            _ticker: ticker,
        });
        cx.notify();
    }

    fn apply(&mut self, event: QueryEvent, first_line: u32) {
        let out = self.output.get_or_insert_with(Output::default);
        match event {
            QueryEvent::Columns(columns) => out.sets.push(ResultSet {
                columns,
                rows: Vec::new(),
            }),
            QueryEvent::Rows(rows) => {
                if let Some(set) = out.sets.last_mut() {
                    set.rows.extend(rows);
                }
            }
            QueryEvent::Done {
                rows_affected: Some(n),
            } => out.message(
                MessageLevel::Info,
                format!("{n} row{} affected", if n == 1 { "" } else { "s" }),
                None,
            ),
            QueryEvent::Done { .. } => {}
            QueryEvent::Message(m) => {
                let text = match m.code {
                    Some(code) => format!("{} ({code})", m.text),
                    None => m.text,
                };
                out.message(m.level, text, None);
            }
            QueryEvent::Finished(Ok(stats)) => {
                out.message(
                    MessageLevel::Info,
                    format!("Finished in {}", elapsed(stats.elapsed)),
                    None,
                );
                // land on the first result, or on the messages if none
                out.shown = 0;
            }
            QueryEvent::Finished(Err(e)) => {
                let line = match &e {
                    DbError::Query { line: Some(l), .. } | DbError::ReadOnly { line: l, .. } => {
                        Some(first_line + l - 1)
                    }
                    _ => None,
                };
                let text = match line {
                    Some(l) => format!("Line {}: {e}", l + 1),
                    None => e.to_string(),
                };
                if e.is_fatal() {
                    self.lost = Some(e.to_string());
                }
                let out = self.output.get_or_insert_with(Output::default);
                out.message(MessageLevel::Error, text, line);
                out.shown = out.sets.len();
            }
        }
    }

    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        if let Some(r) = &self.running {
            r.cancel.cancel();
        }
        cx.notify();
    }

    fn reconnect(&mut self, cx: &mut Context<Self>) {
        let Some(saved) = self.connection.as_ref().map(|c| c.saved.clone()) else {
            return;
        };
        let Some(driver) = connections::driver(saved.driver) else {
            return;
        };
        if self.reconnecting {
            return;
        }
        self.reconnecting = true;
        let state = cx.global::<Connections>();
        let (secrets, runtime) = (state.secrets.clone(), state.runtime.clone());
        let for_key = saved.clone();
        cx.spawn(async move |this, cx| {
            let config = cx
                .background_spawn(async move { for_key.connect_config(secrets.as_ref()) })
                .await;
            let result = match config {
                Ok(config) => {
                    runtime
                        .connect(driver, config, SessionOptions::default())
                        .await
                }
                Err(e) => Err(DbError::Other(e.to_string())),
            };
            this.update(cx, |this, cx| {
                this.reconnecting = false;
                match result {
                    Ok(session) => {
                        this.set_connection(ActiveConnection { saved, session }, cx);
                        this.say(MessageLevel::Info, "Reconnected.", None);
                    }
                    Err(e) => this.say(
                        MessageLevel::Error,
                        &format!("Couldn't reconnect: {e}"),
                        None,
                    ),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn toggle_databases(&mut self, cx: &mut Context<Self>) {
        if self.databases.take().is_some() {
            cx.notify();
            return;
        }
        let Some(conn) = &self.connection else {
            return;
        };
        let session = conn.session.clone();
        cx.spawn(async move |this, cx| {
            let result = session.databases().await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(dbs) => this.databases = Some(dbs),
                    Err(e) => this.say(MessageLevel::Error, &e.to_string(), None),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn use_database(&mut self, name: String, cx: &mut Context<Self>) {
        self.databases = None;
        let Some(conn) = &self.connection else {
            return;
        };
        let session = conn.session.clone();
        cx.spawn(async move |this, cx| {
            let result = session.use_database(name.clone()).await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(()) => this.database = Some(name),
                    Err(e) => this.say(MessageLevel::Error, &e.to_string(), None),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn jump_to_line(&mut self, line: u32, window: &mut Window, cx: &mut Context<Self>) {
        self.editor.update(cx, |e, cx| {
            let at = e.editor.line_start(line as usize);
            e.editor.set_cursor(at);
            e.request_autoscroll(cx);
        });
        window.focus(&self.editor.focus_handle(cx));
    }

    #[cfg(test)]
    pub fn output_text(&self) -> Vec<String> {
        self.output
            .iter()
            .flat_map(|o| o.messages.iter().map(|m| m.text.clone()))
            .collect()
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let chip = |id: &'static str| {
            div()
                .id(id)
                .flex()
                .items_center()
                .gap_1p5()
                .px_2()
                .h(px(22.))
                .rounded_sm()
                .cursor_pointer()
                .hover(|d| d.bg(Theme::border()))
        };
        let connection = match &self.connection {
            Some(c) => chip("tab-connection")
                .child(color_dot(c.saved.color))
                .child(c.saved.name.clone())
                .when(c.saved.read_only, |d| {
                    d.child(div().text_color(Theme::muted()).child("read-only"))
                }),
            None => chip("tab-connection")
                .text_color(Theme::muted())
                .child("Connect…"),
        }
        .on_click(cx.listener(|_, _, _, cx| cx.emit(TabEvent::NeedConnection)));

        let database = self.connection.as_ref().map(|_| {
            chip("tab-database")
                .text_color(Theme::muted())
                .child(
                    self.database
                        .clone()
                        .unwrap_or_else(|| "no database".into()),
                )
                .child("▾")
                .on_click(cx.listener(|this, _, _, cx| this.toggle_databases(cx)))
        });

        let right = if let Some(r) = &self.running {
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .text_color(Theme::muted())
                        .child(format!("Running {}", elapsed(r.started.elapsed()))),
                )
                .child(
                    chip("tab-cancel")
                        .border_1()
                        .border_color(Theme::border())
                        .child("■ Cancel")
                        .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
                )
        } else if let Some(reason) = &self.lost {
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .text_color(Theme::error())
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .child(format!("Connection lost: {reason}")),
                )
                .child(
                    chip("tab-reconnect")
                        .border_1()
                        .border_color(Theme::border())
                        .child(if self.reconnecting {
                            "Reconnecting…"
                        } else {
                            "Reconnect"
                        })
                        .on_click(cx.listener(|this, _, _, cx| this.reconnect(cx))),
                )
        } else {
            div().flex().items_center().gap_1().child(
                chip("tab-run")
                    .border_1()
                    .border_color(Theme::border())
                    .child("▶ Run")
                    .on_click(
                        cx.listener(|this, _, window, cx| this.run_action(false, window, cx)),
                    ),
            )
        };

        div()
            .flex()
            .flex_none()
            .items_center()
            .justify_between()
            .h(px(30.))
            .px_2()
            .gap_2()
            .bg(Theme::panel())
            .border_b_1()
            .border_color(Theme::border())
            .text_size(px(11.5))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(connection)
                    .children(database),
            )
            .child(right)
    }

    fn render_databases(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let dbs = self.databases.as_ref()?;
        Some(
            div()
                .id("database-list")
                .absolute()
                .top(px(30.))
                .left(px(120.))
                .w(px(220.))
                .max_h(px(300.))
                .overflow_y_scroll()
                .py_1()
                .bg(Theme::panel())
                .border_1()
                .border_color(Theme::border())
                .rounded_md()
                .shadow_lg()
                .occlude()
                .children(dbs.iter().cloned().map(|db| {
                    let current = self.database.as_deref() == Some(db.as_str());
                    div()
                        .id(SharedString::from(format!("db-{db}")))
                        .px_3()
                        .py_0p5()
                        .cursor_pointer()
                        .when(current, |d| d.text_color(Theme::accent()))
                        .hover(|d| d.bg(Theme::border()))
                        .child(db.clone())
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.use_database(db.clone(), cx)),
                        )
                })),
        )
    }

    fn render_output(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let out = self.output.as_ref()?;
        let shown = out.shown.min(out.sets.len());
        let tab = |ix: usize, label: String, active: bool| {
            div()
                .id(("output-tab", ix))
                .px_3()
                .h_full()
                .flex()
                .items_center()
                .cursor_pointer()
                .border_b_2()
                .border_color(if active {
                    Theme::accent()
                } else {
                    Theme::panel()
                })
                .text_color(if active {
                    Theme::text()
                } else {
                    Theme::muted()
                })
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(o) = &mut this.output {
                        o.shown = ix;
                    }
                    cx.notify();
                }))
        };
        let mut tabs: Vec<_> = out
            .sets
            .iter()
            .enumerate()
            .map(|(ix, s)| {
                let n = s.rows.len();
                tab(
                    ix,
                    format!(
                        "Result {} · {n} row{}",
                        ix + 1,
                        if n == 1 { "" } else { "s" }
                    ),
                    ix == shown,
                )
            })
            .collect();
        tabs.push(tab(
            out.sets.len(),
            "Messages".into(),
            shown == out.sets.len(),
        ));

        let body = match out.sets.get(shown) {
            Some(set) => self.render_rows(set, shown, cx).into_any_element(),
            None => self.render_messages(out, cx).into_any_element(),
        };
        Some(
            div()
                .flex_none()
                .h(px(PANEL_HEIGHT))
                .flex()
                .flex_col()
                .border_t_1()
                .border_color(Theme::border())
                .bg(Theme::panel())
                .text_size(px(11.5))
                .child(
                    div()
                        .flex()
                        .flex_none()
                        .h(px(26.))
                        .border_b_1()
                        .border_color(Theme::border())
                        .children(tabs),
                )
                .child(div().flex_1().min_h_0().child(body)),
        )
    }

    fn render_rows(&self, set: &ResultSet, ix: usize, cx: &mut Context<Self>) -> impl IntoElement {
        let cell = || {
            div()
                .w(px(CELL_WIDTH))
                .flex_none()
                .px_2()
                .overflow_hidden()
                .whitespace_nowrap()
        };
        let header = div()
            .flex()
            .flex_none()
            .h(px(22.))
            .items_center()
            .border_b_1()
            .border_color(Theme::border())
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .children(set.columns.iter().map(|c| cell().child(c.name.clone())));
        let count = set.rows.len();
        let rows = uniform_list(
            ("rows", ix),
            count,
            cx.processor(move |this, range: Range<usize>, _, _| {
                let Some(set) = this.output.as_ref().and_then(|o| o.sets.get(ix)) else {
                    return Vec::new();
                };
                range
                    .filter_map(|r| set.rows.get(r))
                    .map(|row| {
                        div()
                            .flex()
                            .h(px(20.))
                            .items_center()
                            .children(row.iter().map(|v| {
                                let text = v.display(CELL_BYTES).replace('\n', "⏎");
                                cell()
                                    .when(v.is_null(), |d| d.text_color(Theme::muted()))
                                    .when(v.is_numeric(), |d| d.flex().justify_end())
                                    .child(text)
                            }))
                    })
                    .collect()
            }),
        )
        .flex_1();
        div()
            .size_full()
            .flex()
            .flex_col()
            .font_family(theme::font_family(cx))
            .child(header)
            .child(rows)
    }

    fn render_messages(&self, out: &Output, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("messages")
            .size_full()
            .overflow_y_scroll()
            .p_2()
            .flex()
            .flex_col()
            .gap_1()
            .children(out.messages.iter().enumerate().map(|(ix, m)| {
                let color = match m.level {
                    MessageLevel::Error => Theme::error(),
                    MessageLevel::Warning => theme::c(0xd9c04a),
                    MessageLevel::Info => Theme::text(),
                };
                let line = m.line;
                div()
                    .id(("message", ix))
                    .text_color(color)
                    .when(line.is_some(), |d| {
                        d.cursor_pointer().hover(|d| d.underline())
                    })
                    .child(m.text.clone())
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            if let Some(l) = line {
                                this.jump_to_line(l, window, cx);
                            }
                        }),
                    )
            }))
    }
}

fn elapsed(d: Duration) -> String {
    if d < Duration::from_secs(1) {
        format!("{} ms", d.as_millis())
    } else {
        format!("{:.1} s", d.as_secs_f64())
    }
}

pub fn close_session(conn: ActiveConnection, cx: &mut App) {
    cx.background_spawn(async move { conn.session.close().await })
        .detach();
}

impl Focusable for QueryTab {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for QueryTab {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context("QueryTab")
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .on_action(
                cx.listener(|this, _: &RunStatement, window, cx| {
                    this.run_action(false, window, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &RunAll, window, cx| this.run_action(true, window, cx)),
            )
            .on_action(cx.listener(|this, _: &CancelQuery, _, cx| this.cancel(cx)))
            .child(self.render_toolbar(cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .child(self.editor.clone()),
            )
            .children(self.render_output(cx))
            .children(self.render_databases(cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn editor(text: &str, cursor: usize) -> Editor {
        let mut ed = Editor::new(text);
        ed.set_cursor(cursor);
        ed
    }

    #[test]
    fn statement_at_the_cursor() {
        let sql = "SELECT 1;\n\nSELECT 2\nFROM t;\n";
        let t = target(&editor(sql, 14), DriverKind::MySql, false).unwrap();
        assert_eq!(t.text, "SELECT 2\nFROM t");
        assert_eq!(t.first_line, 2);
        assert_eq!(t.range, 11..26);
        // in the blank line after the first statement: that one
        let t = target(&editor(sql, 10), DriverKind::MySql, false).unwrap();
        assert_eq!(t.text, "SELECT 1");
        // right after the semicolon still counts as the statement
        let t = target(&editor(sql, 9), DriverKind::MySql, false).unwrap();
        assert_eq!(t.text, "SELECT 1");
    }

    #[test]
    fn selection_and_whole_buffer() {
        let sql = "SELECT 1; SELECT 2;";
        let mut ed = editor(sql, 0);
        ed.set_selections(vec![honcho_core::Selection::new(10, 18)], 0);
        let t = target(&ed, DriverKind::MySql, false).unwrap();
        assert_eq!(t.text, "SELECT 2");
        let t = target(&ed, DriverKind::MySql, true).unwrap();
        assert_eq!(t.text, sql);
        assert_eq!(
            target(&editor("  \n", 0), DriverKind::MySql, false),
            Err("Nothing to run.")
        );
        assert_eq!(
            target(&editor("", 0), DriverKind::MySql, true),
            Err("Nothing to run.")
        );
    }

    #[test]
    fn ranges_count_chars_not_bytes() {
        let sql = "SELECT 'ไทย';\nSELECT 2;";
        let t = target(&editor(sql, 16), DriverKind::MySql, false).unwrap();
        assert_eq!(t.text, "SELECT 2");
        assert_eq!(t.range, 14..22);
    }

    #[test]
    fn several_cursors_are_refused() {
        let mut ed = editor("SELECT 1;\nSELECT 2;", 0);
        ed.add_cursor_vertical(true);
        assert!(target(&ed, DriverKind::MySql, false).is_err());
    }
}
