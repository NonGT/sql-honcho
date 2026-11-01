//! A tab's connection and the queries run from it.

use std::ops::Range;
use std::time::{Duration, Instant};

use gpui::{
    actions, div, prelude::*, px, App, Context, CursorStyle, Entity, EventEmitter, FocusHandle,
    Focusable, MouseButton, Pixels, PromptLevel, SharedString, Subscription, Task, Window,
};
use honcho_config::{ColorTag, SavedConnection};
use honcho_core::{complete, Editor};
use honcho_db::script;
use honcho_db::{DbError, DriverKind, MessageLevel, QueryEvent};
use tokio_util::sync::CancellationToken;

use crate::catalog::{self, Catalog, Catalogs};
use crate::connection_dialog::color_dot;
use crate::connections::{self, ActiveConnection};
use crate::editor_view::{self, CompletionMenu, EditorView};
use crate::grid::{Format, ResultGrid};
use crate::theme::{self, Theme};

actions!(
    query,
    [
        RefreshSchema,
        RunStatement,
        RunAll,
        CancelQuery,
        TogglePanel,
        CloseDatabases,
        PrevDatabase,
        NextDatabase,
        PickDatabase,
    ]
);

const PANEL_HEIGHT: Pixels = px(260.);
const PANEL_MIN: Pixels = px(80.);
const PANEL_BAR: Pixels = px(28.);

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

pub struct OutputMessage {
    pub level: MessageLevel,
    pub text: String,
    /// 0-based buffer line to jump to.
    pub line: Option<u32>,
}

#[derive(Default)]
pub struct Output {
    pub sets: Vec<Entity<ResultGrid>>,
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

/// What to do once a reconnect succeeds.
enum After {
    /// Pressed Reconnect.
    Manual,
    /// The connection dropped mid-query, which isn't run again.
    CutOff,
    /// A run that was never sent.
    Run(Target),
    /// The database list that failed to load.
    Databases,
    /// Switching databases; the new session opens on it already.
    UseDatabase,
}

const RECONNECTED: &str = "The server had closed the connection. Reconnected; \
                           session variables and temporary tables start fresh.";

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
    /// Highlighted row in the open database list.
    database_ix: usize,
    database_focus: FocusHandle,
    /// Focus the database list on the next render; it opens after an await.
    focus_databases: bool,
    running: Option<Running>,
    pub output: Option<Output>,
    /// Why the connection is gone, when it is.
    pub lost: Option<String>,
    reconnecting: bool,
    /// A run is waiting on a ping or a reconnect before it starts.
    preparing: bool,
    /// The editor state the run target was worked out for.
    target_key: Option<(u64, usize, Vec<honcho_core::Selection>)>,
    panel_height: Pixels,
    panel_collapsed: bool,
    /// Pointer y and panel height when a resize drag started.
    panel_drag: Option<(Pixels, Pixels)>,
    /// Revision, cursor and length when the editor last changed, to tell
    /// typing from other edits and moves.
    last_edit: (u64, usize, usize),
    /// Completion is wanted: the menu shows, or waits on metadata.
    completing: bool,
    catalog: Option<(Entity<Catalog>, Subscription)>,
    /// The run changes the schema; forget the catalog when it's done.
    ddl_running: bool,
    _subs: Vec<Subscription>,
}

impl EventEmitter<TabEvent> for QueryTab {}

impl QueryTab {
    pub fn new(editor: Entity<EditorView>, cx: &mut Context<Self>) -> Self {
        let subs = vec![cx.observe(&editor, |this, _, cx| {
            this.update_target(cx);
            this.editor_changed(cx);
            cx.notify();
        })];
        Self {
            focus: cx.focus_handle(),
            editor,
            connection: None,
            database: None,
            databases: None,
            database_ix: 0,
            database_focus: cx.focus_handle(),
            focus_databases: false,
            running: None,
            output: None,
            lost: None,
            reconnecting: false,
            preparing: false,
            target_key: None,
            panel_height: PANEL_HEIGHT,
            panel_collapsed: false,
            panel_drag: None,
            last_edit: (0, 0, 0),
            completing: false,
            catalog: None,
            ddl_running: false,
            _subs: subs,
        }
    }

    pub fn is_running(&self) -> bool {
        self.running.is_some() || self.preparing
    }

    fn auto_reconnect(&self) -> bool {
        self.connection
            .as_ref()
            .is_some_and(|c| c.saved.auto_reconnect)
    }

    pub fn color(&self) -> Option<ColorTag> {
        self.connection.as_ref().and_then(|c| c.saved.color)
    }

    fn kind(&self) -> Option<DriverKind> {
        self.connection.as_ref().map(|c| c.saved.driver)
    }

    fn catalog(&mut self, cx: &mut Context<Self>) -> Option<Entity<Catalog>> {
        let saved = self.connection.as_ref()?.saved.clone();
        if self.catalog.is_none() {
            let catalog = Catalogs::get(&saved, cx);
            let sub = cx.observe(&catalog, |this, _, cx| {
                if this.completing {
                    this.refresh_completion(cx);
                }
            });
            self.catalog = Some((catalog, sub));
        }
        self.catalog.as_ref().map(|(c, _)| c.clone())
    }

    fn forget_schema(&mut self, cx: &mut Context<Self>) {
        if let Some(catalog) = self.catalog(cx) {
            catalog.update(cx, |c, cx| c.clear(cx));
        }
    }

    /// Opens completion while a name is being typed and closes it when the
    /// cursor goes elsewhere.
    fn editor_changed(&mut self, cx: &mut Context<Self>) {
        let ed = &self.editor.read(cx).editor;
        let sels = ed.selections();
        let now = (ed.revision(), ed.newest_selection().head, ed.len_chars());
        let prev = std::mem::replace(&mut self.last_edit, now);
        if now == prev {
            // a repaint, or the menu itself changing
            return;
        }
        let single = sels.len() == 1 && sels[0].is_empty();
        let in_word = now.1 > 0
            && ed
                .slice(now.1 - 1..now.1)
                .chars()
                .all(|c| c.is_alphanumeric() || c == '_' || c == '.' || c == '`');
        let typed = now.0 != prev.0 && now.2 == prev.2 + 1 && now.1 == prev.1 + 1;
        let erased = now.0 != prev.0 && now.2 + 1 == prev.2 && now.1 + 1 == prev.1;
        self.completing = single && in_word && (typed || (erased && self.completing));
        if self.completing {
            self.refresh_completion(cx);
        } else {
            self.close_completion(cx);
        }
    }

    fn close_completion(&mut self, cx: &mut Context<Self>) {
        self.completing = false;
        self.editor.update(cx, |e, cx| {
            if e.completion.take().is_some() {
                cx.notify();
            }
        });
    }

    fn refresh_completion(&mut self, cx: &mut Context<Self>) {
        let kind = self.kind().unwrap_or(DriverKind::MySql);
        let catalog = self.catalog(cx);
        let (result, base) = {
            let ed = &self.editor.read(cx).editor;
            let text = ed.text();
            let at = ed.rope().char_to_byte(ed.newest_selection().head);
            // the statement around the cursor, or nothing between statements
            let statements = script::split(kind, &text);
            let (stmt, base) = statements
                .iter()
                .find(|s| s.start <= at && at <= s.start + s.text.len())
                .map_or(("", at), |s| (s.text, s.start));
            let current = self.database.as_deref();
            let result = match &catalog {
                Some(c) => complete::complete(stmt, at - base, &c.read(cx).view(current)),
                None => complete::complete(stmt, at - base, &catalog::View::empty()),
            };
            (result, base)
        };
        let note = catalog
            .as_ref()
            .and_then(|c| c.read(cx).lost().map(str::to_string));
        if let Some(catalog) = &catalog {
            if !result.missing.is_empty() {
                let missing = result.missing.clone();
                catalog.update(cx, |c, cx| c.fetch(&missing, cx));
            }
        }
        self.editor.update(cx, |e, cx| {
            let rope = e.editor.rope();
            let replace = rope.byte_to_char(base + result.replace.start)
                ..rope.byte_to_char(base + result.replace.end);
            // the best match is highlighted, unless the user picked one with
            // the arrows; then that one stays as more names arrive
            let navigated = e.completion.as_ref().is_some_and(|m| m.navigated);
            let selected = e
                .completion
                .as_ref()
                .filter(|m| m.navigated)
                .and_then(|m| m.items.get(m.selected))
                .and_then(|old| result.items.iter().position(|i| i == old))
                .unwrap_or(0);
            // keywords still work without a connection; the note says why
            // the names are missing
            e.completion = (!result.items.is_empty() || note.is_some()).then_some(CompletionMenu {
                items: result.items,
                selected,
                replace,
                navigated,
                note,
            });
            cx.notify();
        });
    }

    pub fn set_connection(&mut self, conn: ActiveConnection, cx: &mut Context<Self>) {
        if let Some(old) = self.connection.replace(conn) {
            close_session(old, cx);
        }
        self.lost = None;
        self.database = None;
        self.databases = None;
        self.target_key = None;
        self.catalog = None;
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

    /// A message that doesn't pull the panel over to the messages tab.
    fn note(&mut self, text: &str) {
        let out = self.output.get_or_insert_with(Output::default);
        out.message(MessageLevel::Info, text, None);
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
        if self.preparing {
            return;
        }
        if self.auto_reconnect() {
            if self.lost.is_some() {
                self.output = Some(Output::default());
                self.preparing = true;
                self.reconnect_then(After::Run(target), cx);
                return;
            }
            // The server drops idle connections (wait_timeout and the like).
            // Find out before sending, so a query is never sent twice.
            let session = conn.session.clone();
            self.preparing = true;
            cx.notify();
            cx.spawn(async move |this, cx| {
                let alive = session.ping().await;
                this.update(cx, |this, cx| match alive {
                    Err(DbError::ConnectionLost(reason)) => {
                        this.lost = Some(reason);
                        this.output = Some(Output::default());
                        this.reconnect_then(After::Run(target), cx);
                    }
                    _ => {
                        this.preparing = false;
                        this.start(target, cx);
                    }
                })
                .ok();
            })
            .detach();
            return;
        }
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
        self.start(target, cx);
    }

    fn start(&mut self, target: Target, cx: &mut Context<Self>) {
        let Some(conn) = &self.connection else {
            return;
        };
        self.ddl_running = script::split(conn.saved.driver, &target.text)
            .iter()
            .any(|s| changes_schema(s.text));
        let mut handle = conn.session.execute(target.text);
        let cancel = handle.cancel_token();
        let first_line = target.first_line;
        self.output = Some(Output::default());
        self.panel_collapsed = false;

        let events = cx.spawn(async move |this, cx| {
            while let Some(event) = handle.next().await {
                let done = matches!(event, QueryEvent::Finished(_));
                let alive = this
                    .update(cx, |this, cx| {
                        this.apply(event, first_line, cx);
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

    fn apply(&mut self, event: QueryEvent, first_line: u32, cx: &mut Context<Self>) {
        let kind = self.kind().unwrap_or(DriverKind::MySql);
        let out = self.output.get_or_insert_with(Output::default);
        match event {
            QueryEvent::Columns(columns) => {
                let grid = cx.new(|cx| ResultGrid::new(kind, columns, cx));
                out.sets.push(grid);
            }
            QueryEvent::Rows(rows) => {
                if let Some(grid) = out.sets.last() {
                    grid.update(cx, |g, cx| g.append(rows, cx));
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
                for grid in &out.sets {
                    grid.update(cx, |g, cx| g.finish(cx));
                }
                if std::mem::take(&mut self.ddl_running) {
                    self.forget_schema(cx);
                }
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
                let out = self.output.get_or_insert_with(Output::default);
                out.message(MessageLevel::Error, text, line);
                out.shown = out.sets.len();
                if let DbError::ConnectionLost(reason) = &e {
                    self.lost = Some(reason.clone());
                    if self.auto_reconnect() {
                        // not run again: it may have done part of its work
                        self.reconnect_then(After::CutOff, cx);
                    }
                }
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
        self.reconnect_then(After::Manual, cx);
    }

    /// Opens a new session, then runs `then` if given. Runs that waited on
    /// this were never sent, so running them now can't repeat anything.
    fn reconnect_then(&mut self, then: After, cx: &mut Context<Self>) {
        let Some(saved) = self.connection.as_ref().map(|c| c.saved.clone()) else {
            self.preparing = false;
            return;
        };
        if connections::driver(saved.driver).is_none() {
            self.preparing = false;
            return;
        }
        if self.reconnecting {
            return;
        }
        self.reconnecting = true;
        // come back to the database the user had moved to
        let opening = connections::open_session(saved.clone(), self.database.clone(), cx);
        cx.spawn(async move |this, cx| {
            let result = opening.await;
            this.update(cx, |this, cx| {
                this.reconnecting = false;
                this.preparing = false;
                match result {
                    Ok(session) => {
                        this.set_connection(ActiveConnection { saved, session }, cx);
                        match then {
                            After::Run(target) => {
                                this.start(target, cx);
                                this.note(RECONNECTED);
                            }
                            After::CutOff => this.say(
                                MessageLevel::Info,
                                "Reconnected. The query that was cut off wasn't run again.",
                                None,
                            ),
                            After::Manual => this.say(MessageLevel::Info, "Reconnected.", None),
                            After::Databases => {
                                this.note(RECONNECTED);
                                this.load_databases(cx);
                            }
                            After::UseDatabase => this.note(RECONNECTED),
                        }
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

    /// Connects a tab opened from somewhere else (the object browser), and
    /// runs the whole buffer once connected when `run`.
    pub fn open_saved(
        &mut self,
        saved: SavedConnection,
        database: Option<String>,
        run: bool,
        cx: &mut Context<Self>,
    ) {
        self.reconnecting = true;
        let opening = connections::open_session(saved.clone(), database, cx);
        cx.spawn(async move |this, cx| {
            let result = opening.await;
            this.update(cx, |this, cx| {
                this.reconnecting = false;
                match result {
                    Ok(session) => {
                        this.set_connection(ActiveConnection { saved, session }, cx);
                        if run {
                            let kind = this.kind().unwrap_or(DriverKind::MySql);
                            if let Ok(t) = target(&this.editor.read(cx).editor, kind, true) {
                                this.run(t, cx);
                            }
                        }
                    }
                    Err(e) => {
                        this.say(MessageLevel::Error, &format!("Couldn't connect: {e}"), None)
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn toggle_databases(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.databases.is_some() {
            self.close_databases(window, cx);
        } else {
            self.load_databases(cx);
        }
    }

    fn load_databases(&mut self, cx: &mut Context<Self>) {
        let Some(conn) = &self.connection else {
            return;
        };
        let session = conn.session.clone();
        cx.spawn(async move |this, cx| {
            let result = session.databases().await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(dbs) => {
                        this.database_ix = this
                            .database
                            .as_ref()
                            .and_then(|d| dbs.iter().position(|x| x == d))
                            .unwrap_or(0);
                        this.databases = Some(dbs);
                        this.focus_databases = true;
                    }
                    // listing is a read, so asking again on a new session is safe
                    Err(DbError::ConnectionLost(reason)) if this.auto_reconnect() => {
                        this.lost = Some(reason);
                        this.reconnect_then(After::Databases, cx);
                    }
                    Err(e) => this.say(MessageLevel::Error, &e.to_string(), None),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn close_databases(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.databases = None;
        window.focus(&self.editor.focus_handle(cx));
        cx.notify();
    }

    fn move_database(&mut self, delta: isize, cx: &mut Context<Self>) {
        if let Some(dbs) = &self.databases {
            let last = dbs.len().saturating_sub(1) as isize;
            self.database_ix = (self.database_ix as isize + delta).clamp(0, last) as usize;
            cx.notify();
        }
    }

    fn pick_database(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        self.close_databases(window, cx);
        let Some(conn) = &self.connection else {
            return;
        };
        let session = conn.session.clone();
        cx.spawn(async move |this, cx| {
            let result = session.use_database(name.clone()).await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(()) => this.database = Some(name),
                    Err(DbError::ConnectionLost(reason)) if this.auto_reconnect() => {
                        // the new session opens on the chosen database
                        this.database = Some(name);
                        this.lost = Some(reason);
                        this.reconnect_then(After::UseDatabase, cx);
                    }
                    Err(e) => this.say(MessageLevel::Error, &e.to_string(), None),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
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
                .children(c.saved.ssh.as_ref().map(|ssh| {
                    div()
                        .text_color(Theme::muted())
                        .child(format!("via {}", ssh.host))
                }))
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
                .on_click(cx.listener(|this, _, window, cx| this.toggle_databases(window, cx)))
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
                .flex_1()
                .min_w_0()
                .justify_end()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .min_w_0()
                        .text_color(Theme::error())
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .child(format!("Connection lost: {reason}")),
                )
                .child(
                    chip("tab-reconnect")
                        .flex_none()
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
        let list =
            div()
                .id("database-list")
                .key_context("DatabaseList")
                .track_focus(&self.database_focus)
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
                .on_action(cx.listener(|this, _: &CloseDatabases, window, cx| {
                    this.close_databases(window, cx)
                }))
                .on_action(cx.listener(|this, _: &PrevDatabase, _, cx| this.move_database(-1, cx)))
                .on_action(cx.listener(|this, _: &NextDatabase, _, cx| this.move_database(1, cx)))
                .on_action(cx.listener(|this, _: &PickDatabase, window, cx| {
                    let name = this
                        .databases
                        .as_ref()
                        .and_then(|d| d.get(this.database_ix).cloned());
                    if let Some(name) = name {
                        this.pick_database(name, window, cx);
                    }
                }))
                .children(dbs.iter().cloned().enumerate().map(|(ix, db)| {
                    let current = self.database.as_deref() == Some(db.as_str());
                    div()
                        .id(SharedString::from(format!("db-{db}")))
                        .px_3()
                        .py_0p5()
                        .cursor_pointer()
                        .when(ix == self.database_ix, |d| d.bg(Theme::accent_dim()))
                        .when(current, |d| d.text_color(Theme::accent()))
                        .hover(|d| d.bg(Theme::border()))
                        .child(db.clone())
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.pick_database(db.clone(), window, cx)
                        }))
                }));
        // a click anywhere outside the list closes it
        let backdrop = div()
            .id("database-backdrop")
            .absolute()
            .size_full()
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| this.close_databases(window, cx)),
            );
        Some(div().absolute().size_full().child(backdrop).child(list))
    }

    fn shown_grid(&self) -> Option<Entity<ResultGrid>> {
        let out = self.output.as_ref()?;
        out.sets.get(out.shown).cloned()
    }

    fn copy_as(&mut self, format: Format, cx: &mut Context<Self>) {
        if let Some(grid) = self.shown_grid() {
            let text = grid.read(cx).export(format);
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
        }
    }

    fn export(&mut self, format: Format, cx: &mut Context<Self>) {
        let Some(grid) = self.shown_grid() else {
            return;
        };
        let dir = directories::UserDirs::new()
            .and_then(|d| {
                d.download_dir()
                    .map(|p| p.to_path_buf())
                    .or(Some(d.home_dir().to_path_buf()))
            })
            .unwrap_or_default();
        let name = format!("result.{}", format.extension());
        let path = cx.prompt_for_new_path(&dir, Some(&name));
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(path))) = path.await else {
                return;
            };
            let Ok(text) = grid.read_with(cx, |g, _| g.export_all(format)) else {
                return;
            };
            let shown = path.display().to_string();
            let written = cx
                .background_spawn(async move { std::fs::write(&path, text) })
                .await;
            this.update(cx, |this, cx| {
                match written {
                    Ok(()) => this.say(MessageLevel::Info, &format!("Saved {shown}"), None),
                    Err(e) => this.say(
                        MessageLevel::Error,
                        &format!("Couldn't save {shown}: {e}"),
                        None,
                    ),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
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
                .flex_none()
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
                    this.panel_collapsed = false;
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
            .map(|(ix, grid)| {
                let n = grid.read(cx).rows.len();
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

        let small = |id: &'static str, label: &'static str| {
            div()
                .id(id)
                .px_1p5()
                .rounded_sm()
                .cursor_pointer()
                .text_color(Theme::muted())
                .hover(|d| d.bg(Theme::border()).text_color(Theme::text()))
                .child(label)
        };
        let has_grid = shown < out.sets.len();
        let actions = div()
            .flex()
            .flex_none()
            .items_center()
            .gap_0p5()
            .px_2()
            .when(has_grid, |d| {
                d.child(div().text_color(Theme::gutter_text()).pr_1().child("Copy"))
                    .children(
                        [Format::Tsv, Format::Csv, Format::Json, Format::Insert].map(|f| {
                            small(f.label(), f.label())
                                .on_click(cx.listener(move |this, _, _, cx| this.copy_as(f, cx)))
                        }),
                    )
                    .child(
                        div()
                            .text_color(Theme::gutter_text())
                            .pl_2()
                            .pr_1()
                            .child("Save"),
                    )
                    .children(
                        [("save-csv", Format::Csv), ("save-json", Format::Json)].map(|(id, f)| {
                            small(id, f.label())
                                .on_click(cx.listener(move |this, _, _, cx| this.export(f, cx)))
                        }),
                    )
            })
            .child(
                small("toggle-panel", if self.panel_collapsed { "▴" } else { "▾" })
                    .ml_2()
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_panel(cx))),
            );

        let body = match out.sets.get(shown) {
            Some(grid) => grid.clone().into_any_element(),
            None => self.render_messages(out, cx).into_any_element(),
        };
        let height = if self.panel_collapsed {
            PANEL_BAR
        } else {
            self.panel_height
        };
        Some(
            div()
                .flex_none()
                .h(height)
                .flex()
                .flex_col()
                .relative()
                .border_t_1()
                .border_color(Theme::border())
                .bg(Theme::panel())
                .text_size(px(11.5))
                .child(
                    div()
                        .flex()
                        .flex_none()
                        .h(PANEL_BAR)
                        .justify_between()
                        .border_b_1()
                        .border_color(Theme::border())
                        .child(
                            div()
                                .id("output-tabs")
                                .flex()
                                .overflow_x_scroll()
                                .children(tabs),
                        )
                        .child(actions),
                )
                .when(!self.panel_collapsed, |d| {
                    d.child(div().flex_1().min_h_0().child(body))
                })
                // drag the top edge to resize
                .child(
                    div()
                        .id("panel-resize")
                        .absolute()
                        .top(px(-3.))
                        .left_0()
                        .right_0()
                        .h(px(6.))
                        .cursor(CursorStyle::ResizeUpDown)
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, e: &gpui::MouseDownEvent, _, cx| {
                                this.panel_collapsed = false;
                                this.panel_drag = Some((e.position.y, this.panel_height));
                                cx.stop_propagation();
                            }),
                        ),
                ),
        )
    }

    #[cfg(test)]
    pub fn catalog_for_test(&self) -> Option<Entity<Catalog>> {
        self.catalog.as_ref().map(|(c, _)| c.clone())
    }

    #[cfg(test)]
    pub fn databases_open(&self) -> bool {
        self.databases.is_some()
    }

    #[cfg(test)]
    pub fn set_auto_reconnect(&mut self, on: bool) {
        if let Some(c) = &mut self.connection {
            c.saved.auto_reconnect = on;
        }
    }

    #[cfg(test)]
    pub fn open_databases(&mut self, cx: &mut Context<Self>) {
        self.load_databases(cx);
    }

    #[cfg(test)]
    pub fn panel_collapsed(&self) -> bool {
        self.panel_collapsed
    }

    fn toggle_panel(&mut self, cx: &mut Context<Self>) {
        self.panel_collapsed = !self.panel_collapsed;
        cx.notify();
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

/// Statements after which the catalog is out of date.
fn changes_schema(statement: &str) -> bool {
    let first = statement
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_uppercase();
    matches!(first.as_str(), "CREATE" | "ALTER" | "DROP" | "RENAME")
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.focus_databases) {
            window.focus(&self.database_focus);
        }
        let max_panel = window.viewport_size().height - px(200.);
        div()
            .key_context("QueryTab")
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .on_mouse_move(cx.listener(move |this, e: &gpui::MouseMoveEvent, _, cx| {
                if let Some((y, h)) = this.panel_drag {
                    this.panel_height = (h + y - e.position.y).max(PANEL_MIN).min(max_panel);
                    cx.notify();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.panel_drag = None),
            )
            .on_action(cx.listener(|this, _: &TogglePanel, _, cx| this.toggle_panel(cx)))
            .on_action(
                cx.listener(|this, _: &editor_view::ShowCompletions, _, cx| {
                    this.completing = true;
                    this.refresh_completion(cx);
                }),
            )
            .on_action(cx.listener(|this, _: &RefreshSchema, _, cx| this.forget_schema(cx)))
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
