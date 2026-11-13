//! A tab's connection and the queries run from it.

use std::cell::Cell;
use std::ops::Range;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{
    actions, anchored, canvas, deferred, div, point, prelude::*, px, App, Context, CursorStyle,
    Entity, EventEmitter, FocusHandle, Focusable, MouseButton, PathBuilder, Pixels, PromptLevel,
    SharedString, Subscription, Task, Window,
};
use honcho_config::{ColorTag, SavedConnection};
use honcho_core::{complete, Editor};
use honcho_db::script;
use honcho_db::{DbError, DriverKind, MessageLevel, ObjectRef, QueryEvent, RunMode};
use tokio_util::sync::CancellationToken;

use crate::catalog::{self, Catalog, Catalogs};
use crate::connection_dialog::color_dot;
use crate::connections::{self, ActiveConnection};
use crate::editor_view::{self, CompletionMenu, EditorView};
use crate::grid::{Format, ResultGrid};
use crate::plan_view::PlanView;
use crate::theme::{self, Theme};
use honcho_db::routine;

actions!(
    query,
    [
        RefreshSchema,
        RunStatement,
        RunAll,
        Explain,
        ExplainAnalyze,
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
    pub mode: RunMode,
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
            mode: RunMode::Execute,
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
            mode: RunMode::Execute,
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
        mode: RunMode::Execute,
    })
}

pub enum TabEvent {
    /// Run or Explain was asked for without a connection: use one that's
    /// already open, or ask.
    NeedConnection,
    /// "Other connection…" in the list: the connections dialog.
    OtherConnection,
    /// An Apply went through: the database's objects changed.
    Applied {
        conn_id: String,
        database: Option<String>,
    },
    /// Open a new tab with this text.
    OpenScript(String),
    /// Connected to a different connection, or moved to another database.
    Connected {
        conn_id: String,
        database: Option<String>,
    },
    /// The connection chip was clicked: offer the open connections.
    PickConnection,
}

/// The server object a tab's script recreates, and its definition when the
/// tab got it, to notice someone else changing it meanwhile.
#[derive(Clone)]
pub struct Origin {
    pub object: ObjectRef,
    pub definition: String,
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
    pub plan: Option<Entity<PlanView>>,
    pub messages: Vec<OutputMessage>,
    /// Which sub-tab shows: a result set, then the plan, then the messages.
    pub shown: usize,
}

impl Output {
    fn plan_ix(&self) -> Option<usize> {
        self.plan.as_ref().map(|_| self.sets.len())
    }

    pub fn messages_ix(&self) -> usize {
        self.sets.len() + usize::from(self.plan.is_some())
    }

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
    /// Bumped when the tab switches database, so an older answer to "which
    /// database is this" arriving late doesn't undo the switch.
    database_gen: u64,
    databases: Option<Vec<String>>,
    /// Highlighted row in the open database list.
    database_ix: usize,
    database_focus: FocusHandle,
    /// The connection list, with the database to open each on.
    connections: Option<Vec<(SavedConnection, Option<String>)>>,
    connection_ix: usize,
    connection_focus: FocusHandle,
    /// Focus the database list on the next render; it opens after an await.
    focus_databases: bool,
    focus_connections: bool,
    running: Option<Running>,
    pub output: Option<Output>,
    /// Why the connection is gone, when it is.
    pub lost: Option<String>,
    reconnecting: bool,
    /// Run or Explain (whole tab, mode) asked for before there was a
    /// connection, to go ahead once there is one.
    pending: Option<(bool, RunMode)>,
    /// A run is waiting on a ping or a reconnect before it starts.
    preparing: bool,
    /// The editor state the run target was worked out for.
    target_key: Option<(u64, usize, Vec<honcho_core::Selection>)>,
    panel_height: Pixels,
    panel_collapsed: bool,
    /// Pointer y and panel height when a resize drag started.
    panel_drag: Option<(Pixels, Pixels)>,
    /// Width of the plan's details column, kept from one explain to the next.
    plan_width: Rc<Cell<Pixels>>,
    /// A short note over the results, such as what was copied, and the
    /// timer that clears it.
    toast: Option<(SharedString, Task<()>)>,
    /// Revision, cursor and length when the editor last changed, to tell
    /// typing from other edits and moves.
    last_edit: (u64, usize, usize),
    /// Completion is wanted: the menu shows, or waits on metadata.
    completing: bool,
    catalog: Option<(Entity<Catalog>, Subscription)>,
    /// The run changes the schema; forget the catalog when it's done.
    ddl_running: bool,
    pub origin: Option<Origin>,
    /// The current run is an Apply of `origin`.
    applying: bool,
    /// The menu under the Explain button is open.
    explain_menu: bool,
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
            database_gen: 0,
            databases: None,
            database_ix: 0,
            database_focus: cx.focus_handle(),
            connections: None,
            connection_ix: 0,
            connection_focus: cx.focus_handle(),
            focus_databases: false,
            focus_connections: false,
            running: None,
            output: None,
            lost: None,
            reconnecting: false,
            pending: None,
            preparing: false,
            target_key: None,
            panel_height: PANEL_HEIGHT,
            panel_collapsed: false,
            panel_drag: None,
            plan_width: Rc::new(Cell::new(crate::plan_view::DETAILS_MIN)),
            toast: None,
            last_edit: (0, 0, 0),
            completing: false,
            catalog: None,
            ddl_running: false,
            origin: None,
            applying: false,
            explain_menu: false,
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
            let missing = result.missing.clone();
            catalog.update(cx, |c, cx| {
                if !missing.is_empty() {
                    c.fetch(&missing, cx);
                }
                c.revalidate(cx);
            });
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

    /// Runs the script that replaces `origin`, after checking nobody
    /// changed it on the server since the tab got it.
    fn apply_origin(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(conn), Some(origin)) = (self.connection.clone(), self.origin.clone()) else {
            cx.emit(TabEvent::NeedConnection);
            return;
        };
        if self.running.is_some() || self.preparing {
            return;
        }
        let name = routine::qualified(conn.saved.driver, &origin.object);
        cx.spawn_in(window, async move |this, cx| {
            let now = conn.session.definition(origin.object.clone()).await;
            let worry = match &now {
                Ok(d) if routine::same_definition(d, &origin.definition) => None,
                Ok(_) => Some(format!("{name} changed on the server since you opened it.")),
                Err(_) => Some(format!(
                    "{name} is no longer on the server, or can't be read."
                )),
            };
            if let Some(message) = worry {
                let Ok(answer) = this.update_in(cx, |_, window, cx| {
                    window.prompt(
                        PromptLevel::Warning,
                        &message,
                        Some("Applying replaces whatever is there now with this script."),
                        // Cancel first so Enter doesn't confirm by reflex
                        &["Cancel", "Apply anyway"],
                        cx,
                    )
                }) else {
                    return;
                };
                if answer.await != Ok(1) {
                    return;
                }
            }
            // where the script has to drop first, make sure its CREATE
            // works before anything is dropped
            let Ok(script) = this.update(cx, |this, cx| this.editor.read(cx).editor.text()) else {
                return;
            };
            if let Some((create, drop)) = routine::trial(conn.saved.driver, &origin.object, &script)
            {
                let tried = run_to_end(&conn.session, create).await;
                // the trial can fail after creating, as SQLite's view check does
                let _ = run_to_end(&conn.session, drop).await;
                if let Err(e) = tried {
                    this.update(cx, |this, cx| {
                        this.say(
                            MessageLevel::Error,
                            &format!(
                                "This script wouldn't create {name}, so nothing was dropped: {e}"
                            ),
                            None,
                        );
                        if definer_refused(&e) {
                            this.note(DEFINER_HINT);
                        }
                        cx.notify();
                    })
                    .ok();
                    return;
                }
            }
            this.update(cx, |this, cx| {
                let kind = this.kind().unwrap_or(DriverKind::MySql);
                if let Ok(t) = target(&this.editor.read(cx).editor, kind, true) {
                    this.applying = true;
                    this.run(t, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// After an Apply: remember the new definition as the one opened, and
    /// let the browser refresh.
    fn applied(&mut self, cx: &mut Context<Self>) {
        let (Some(conn), Some(origin)) = (self.connection.clone(), self.origin.clone()) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let now = conn.session.definition(origin.object.clone()).await;
            this.update(cx, |this, cx| {
                if let (Ok(def), Some(o)) = (now, &mut this.origin) {
                    o.definition = def;
                }
                this.say(MessageLevel::Info, "Applied.", None);
                cx.emit(TabEvent::Applied {
                    conn_id: conn.saved.id.clone(),
                    database: origin.object.database.clone(),
                });
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// MySQL can't roll back DDL: when an Apply dropped the object and then
    /// failed to create it, put the original back.
    fn apply_failed(&mut self, error: &DbError, cx: &mut Context<Self>) {
        let (Some(conn), Some(origin)) = (self.connection.clone(), self.origin.clone()) else {
            return;
        };
        if definer_refused(error) {
            self.note(DEFINER_HINT);
        }
        let kind = conn.saved.driver;
        let name = routine::qualified(kind, &origin.object);
        let restore = routine::restore_script(kind, &origin.object, &origin.definition);
        cx.spawn(async move |this, cx| {
            if conn.session.definition(origin.object.clone()).await.is_ok() {
                // still there: nothing was lost
                return;
            }
            let mut handle = conn.session.execute(restore.clone());
            let mut put_back = false;
            while let Some(event) = handle.next().await {
                if let QueryEvent::Finished(r) = event {
                    put_back = r.is_ok();
                }
            }
            this.update(cx, |this, cx| {
                if put_back {
                    this.say(
                        MessageLevel::Warning,
                        &format!("Apply failed, so the original {name} was put back."),
                        None,
                    );
                } else {
                    this.say(
                        MessageLevel::Error,
                        &format!(
                            "Apply failed and {name} couldn't be put back. \
                             Its original definition is in a new tab."
                        ),
                        None,
                    );
                    cx.emit(TabEvent::OpenScript(restore));
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub fn set_connection(&mut self, conn: ActiveConnection, cx: &mut Context<Self>) {
        let moved = self.connection.as_ref().map(|c| &c.saved.id) != Some(&conn.saved.id);
        if let Some(old) = self.connection.replace(conn) {
            close_session(old, cx);
        }
        self.lost = None;
        self.database = None;
        self.databases = None;
        self.target_key = None;
        self.catalog = None;
        self.update_target(cx);
        self.refresh_database(moved, cx);
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

    /// Asks the session which database it's on; `announce` tells the
    /// workspace once it knows.
    fn refresh_database(&mut self, announce: bool, cx: &mut Context<Self>) {
        let Some(conn) = &self.connection else {
            return;
        };
        let (session, conn_id) = (conn.session.clone(), conn.saved.id.clone());
        let gen = self.database_gen;
        cx.spawn(async move |this, cx| {
            if let Ok(db) = session.current_database().await {
                this.update(cx, |this, cx| {
                    if this.database_gen != gen {
                        return;
                    }
                    if announce {
                        cx.emit(TabEvent::Connected {
                            conn_id,
                            database: db.clone(),
                        });
                    }
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
        self.start_action(whole, RunMode::Execute, window, cx);
    }

    fn explain_action(&mut self, analyze: bool, window: &mut Window, cx: &mut Context<Self>) {
        // from the toolbar button, typing should still go to the editor
        window.focus(&self.editor.focus_handle(cx));
        self.start_action(false, RunMode::Explain { analyze }, window, cx);
    }

    fn start_action(
        &mut self,
        whole: bool,
        mode: RunMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(conn) = self.connection.clone() else {
            self.pending = Some((whole, mode));
            cx.emit(TabEvent::NeedConnection);
            return;
        };
        if self.running.is_some() {
            return;
        }
        // running part of it could drop the object without creating it again
        if let (Some(o), RunMode::Execute) = (&self.origin, mode) {
            let name = routine::qualified(conn.saved.driver, &o.object);
            self.output.get_or_insert_with(Output::default);
            self.say(
                MessageLevel::Info,
                &format!("This tab replaces {name} on the server: use Apply to run it."),
                None,
            );
            cx.notify();
            return;
        }
        if let RunMode::Explain { analyze: true } = mode {
            if let Some(why) = self.no_analyze() {
                self.output.get_or_insert_with(Output::default);
                self.say(MessageLevel::Warning, why, None);
                cx.notify();
                return;
            }
        }
        let target = match target(&self.editor.read(cx).editor, conn.saved.driver, whole) {
            Ok(t) => Target { mode, ..t },
            Err(e) => {
                self.output = Some(Output::default());
                self.say(MessageLevel::Warning, e, None);
                return;
            }
        };
        // a read-only session refuses an analyzed write itself; no need to ask
        if let (RunMode::Explain { analyze: true }, false) = (mode, conn.saved.read_only) {
            let red = conn.saved.color == Some(ColorTag::Red);
            if let Some((title, detail, run)) = analyze_warning(&target.text, &conn.saved.name, red)
            {
                let answer = window.prompt(
                    PromptLevel::Warning,
                    &title,
                    Some(&detail),
                    // Cancel first so Enter doesn't confirm by reflex
                    &["Cancel", run],
                    cx,
                );
                cx.spawn_in(window, async move |this, cx| {
                    if answer.await == Ok(1) {
                        this.update(cx, |this, cx| this.run(target, cx)).ok();
                    }
                })
                .detach();
                return;
            }
        }
        // a read-only session refuses these itself
        let warning = (mode == RunMode::Execute && !conn.saved.read_only)
            .then(|| change_warning(conn.saved.driver, &target, &conn.saved.name))
            .flatten();
        let Some((title, detail)) = warning else {
            self.run(target, cx);
            return;
        };
        let answer = window.prompt(
            PromptLevel::Warning,
            &title,
            Some(&detail),
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
        out.shown = out.messages_ix();
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
        self.ddl_running = target.mode == RunMode::Execute
            && script::split(conn.saved.driver, &target.text)
                .iter()
                .any(|s| changes_schema(s.text));
        let mut handle = conn.session.run(target.text, target.mode);
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
                this.refresh_database(false, cx);
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
            QueryEvent::Plan(plan) => {
                let width = self.plan_width.clone();
                out.plan = Some(cx.new(|_| PlanView::new(*plan).with_details_width(width)));
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
                if std::mem::take(&mut self.applying) {
                    self.applied(cx);
                }
            }
            QueryEvent::Finished(Err(e)) => {
                if std::mem::take(&mut self.applying) {
                    self.apply_failed(&e, cx);
                }
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
                out.shown = out.messages_ix();
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
                        this.pending = None;
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

    pub(crate) fn pick_database(
        &mut self,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_databases(window, cx);
        let Some(conn) = &self.connection else {
            return;
        };
        if self.database.as_ref() == Some(&name) {
            return;
        }
        // where switching means a new connection, the old one's
        // transaction goes with it
        if conn.session.capabilities().cross_database {
            self.use_database(name, cx);
        } else {
            self.after_leaving(window, cx, move |this, _, cx| this.use_database(name, cx));
        }
    }

    /// Connects this tab elsewhere, once its open transaction, if any, may
    /// be rolled back.
    pub fn switch_to(
        &mut self,
        saved: SavedConnection,
        database: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.after_leaving(window, cx, move |this, _, cx| {
            this.open_saved(saved, database, false, cx)
        });
    }

    /// Runs `then` straight away when closing this tab's session loses
    /// nothing, or else once the user agrees to roll back its transaction.
    fn after_leaving(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) {
        let Some(conn) = &self.connection else {
            return then(self, window, cx);
        };
        let session = conn.session.clone();
        cx.spawn_in(window, async move |this, cx| {
            // a session that can't answer has nothing worth keeping
            if session.in_transaction().await.unwrap_or(false) {
                let Ok(answer) = this.update_in(cx, |_, window, cx| {
                    window.prompt(
                        PromptLevel::Warning,
                        "This tab has a transaction open.",
                        Some(
                            "Switching closes its connection, which rolls the transaction \
                             back. Temp tables and SET options go with it.",
                        ),
                        &["Cancel", "Roll back and switch"],
                        cx,
                    )
                }) else {
                    return;
                };
                if answer.await != Ok(1) {
                    return;
                }
            }
            this.update_in(cx, |this, window, cx| then(this, window, cx))
                .ok();
        })
        .detach();
    }

    fn use_database(&mut self, name: String, cx: &mut Context<Self>) {
        let Some(conn) = &self.connection else {
            return;
        };
        let session = conn.session.clone();
        self.database_gen += 1;
        cx.spawn(async move |this, cx| {
            let result = session.use_database(name.clone()).await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(()) => {
                        if let Some(c) = &this.connection {
                            cx.emit(TabEvent::Connected {
                                conn_id: c.saved.id.clone(),
                                database: Some(name.clone()),
                            });
                        }
                        this.database = Some(name)
                    }
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

    /// Why Explain Analyze can't run on this connection, if it can't.
    fn no_analyze(&self) -> Option<&'static str> {
        let conn = self.connection.as_ref()?;
        let caps = conn.session.capabilities();
        if !caps.explain || caps.explain_analyze {
            return None;
        }
        Some(match conn.saved.driver {
            DriverKind::Sqlite => "SQLite has no Explain Analyze. Explain shows the plan.",
            DriverKind::MySql => "Explain Analyze needs MySQL 8.3 or later.",
            _ => "Explain Analyze isn't available on this connection.",
        })
    }

    fn can_explain(&self) -> bool {
        self.connection
            .as_ref()
            .is_some_and(|c| c.session.capabilities().explain)
    }

    /// Explain next to Run; Explain Analyze runs the statement, so it sits
    /// in the menu under ▾ rather than a click away.
    fn render_explain(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // without a connection a click asks for one, like Run
        let enabled = (self.connection.is_none() || self.can_explain()) && self.running.is_none();
        let part = |id: &'static str| {
            div()
                .id(id)
                .debug_selector(move || id.into())
                .flex()
                .items_center()
                .h(px(22.))
                .px_2()
                .border_1()
                .border_color(Theme::border())
                .when(enabled, |d| {
                    d.cursor_pointer().hover(|d| d.bg(Theme::border()))
                })
                .when(!enabled, |d| d.text_color(Theme::muted()))
        };
        let why = self
            .connection
            .as_ref()
            .filter(|c| !c.session.capabilities().explain)
            .map(|c| {
                SharedString::from(format!(
                    "Explain isn't available for {} yet",
                    crate::connections::driver_name(c.saved.driver)
                ))
            });
        let tip = move |d: gpui::Stateful<gpui::Div>| match why.clone() {
            Some(text) => d.tooltip(move |_, cx| cx.new(|_| Tip(text.clone())).into()),
            None => d,
        };
        let p = crate::keymap::Platform::current();
        let item = |id: &'static str, label: &'static str, action: Box<dyn gpui::Action>| {
            let shortcut = crate::commands::shortcut(&*action, p);
            div()
                .id(id)
                .debug_selector(move || id.into())
                .flex()
                .gap_6()
                .px_3()
                .py_0p5()
                .rounded_sm()
                .cursor_pointer()
                .hover(|d| d.bg(Theme::accent_dim()))
                .child(div().flex_1().child(label))
                .children(shortcut.map(|s| div().text_color(Theme::muted()).child(s)))
        };
        let menu = div()
            .occlude()
            .min_w(px(220.))
            .p_1()
            .bg(Theme::panel())
            .border_1()
            .border_color(Theme::border())
            .rounded_md()
            .shadow_lg()
            .text_size(px(12.))
            .text_color(Theme::text())
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.explain_menu = false;
                cx.notify();
            }))
            .child(
                item("explain-plain", "Explain", Box::new(Explain)).on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, window, cx| {
                        cx.stop_propagation();
                        this.explain_menu = false;
                        this.explain_action(false, window, cx);
                    }),
                ),
            )
            .child(
                item(
                    "explain-analyze",
                    "Explain Analyze…",
                    Box::new(ExplainAnalyze),
                )
                .when(self.no_analyze().is_some(), |d| {
                    d.text_color(Theme::muted())
                })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, window, cx| {
                        cx.stop_propagation();
                        this.explain_menu = false;
                        this.explain_action(true, window, cx);
                    }),
                ),
            );
        div()
            .relative()
            .flex()
            .items_center()
            .child(tip(part("tab-explain")
                .rounded_l_sm()
                .child("Explain")
                .when(enabled, |d| {
                    d.on_click(
                        cx.listener(|this, _, window, cx| this.explain_action(false, window, cx)),
                    )
                })))
            .child(tip(part("tab-explain-menu")
                .rounded_r_sm()
                .border_l_0()
                .px_1()
                .child("▾")
                .when(enabled, |d| {
                    d.on_click(cx.listener(|this, _, _, cx| {
                        this.explain_menu = !this.explain_menu;
                        cx.notify();
                    }))
                })))
            .when(self.explain_menu && enabled, |d| {
                d.child(
                    div()
                        .absolute()
                        .top_full()
                        .right_0()
                        .child(deferred(anchored().child(menu)).with_priority(1)),
                )
            })
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
        .on_click(cx.listener(|this, _, window, cx| {
            if this.connections.is_some() {
                this.close_connections(window, cx);
            } else {
                cx.emit(TabEvent::PickConnection);
            }
        }));

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
            div()
                .flex()
                .items_center()
                .gap_1()
                .when(self.origin.is_some(), |d| {
                    d.child(
                        chip("tab-apply")
                            .border_1()
                            .border_color(Theme::accent())
                            .child("Apply")
                            .on_click(
                                cx.listener(|this, _, window, cx| this.apply_origin(window, cx)),
                            ),
                    )
                })
                .when(self.origin.is_none(), |d| d.child(self.render_explain(cx)))
                .when(self.origin.is_none(), |d| {
                    d.child(
                        chip("tab-run")
                            .border_1()
                            .border_color(Theme::border())
                            .child(play_icon())
                            .child("Run")
                            .on_click(cx.listener(|this, _, window, cx| {
                                window.focus(&this.editor.focus_handle(cx));
                                this.run_action(false, window, cx)
                            })),
                    )
                })
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
                    .children(database)
                    .children(self.origin.as_ref().map(|o| {
                        let kind = self.kind().unwrap_or(DriverKind::MySql);
                        div()
                            .pl_2()
                            .text_color(Theme::muted())
                            .child(format!("editing {}", routine::qualified(kind, &o.object)))
                    })),
            )
            .child(right)
    }

    /// Shows the connection list; the workspace fills it with what the
    /// object browser has open.
    pub fn show_connections(
        &mut self,
        list: Vec<(SavedConnection, Option<String>)>,
        cx: &mut Context<Self>,
    ) {
        self.databases = None;
        let current = self.connection.as_ref().map(|c| c.saved.id.clone());
        self.connection_ix = list
            .iter()
            .position(|(s, _)| Some(&s.id) == current.as_ref())
            .unwrap_or(0);
        self.connections = Some(list);
        self.focus_connections = true;
        cx.notify();
    }

    fn close_connections(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.connections = None;
        self.pending = None;
        window.focus(&self.editor.focus_handle(cx));
        cx.notify();
    }

    fn move_connection(&mut self, delta: isize, cx: &mut Context<Self>) {
        if let Some(list) = &self.connections {
            // one past the end is "Other connection…"
            let last = list.len() as isize;
            self.connection_ix = (self.connection_ix as isize + delta).clamp(0, last) as usize;
            cx.notify();
        }
    }

    /// Opens a session on the picked entry; past the end means the dialog.
    pub fn pick_connection(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let picked = self.connections.as_ref().and_then(|l| l.get(ix).cloned());
        let pending = self.pending.take();
        self.close_connections(window, cx);
        self.pending = pending;
        match picked {
            Some((saved, database)) => self.switch_to(saved, database, window, cx),
            None => cx.emit(TabEvent::OtherConnection),
        }
    }

    /// Drops a Run or Explain that was waiting on a connection.
    pub fn forget_pending(&mut self) {
        self.pending = None;
    }

    #[cfg(test)]
    pub fn connection_names(&self) -> Option<Vec<String>> {
        self.connections
            .as_ref()
            .map(|l| l.iter().map(|(s, _)| s.name.clone()).collect())
    }

    fn render_connections(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let list = self.connections.as_ref()?;
        let current = self.connection.as_ref().map(|c| c.saved.id.clone());
        let row = |id: SharedString, ix: usize| {
            div()
                .id(id)
                .flex()
                .items_center()
                .gap_1p5()
                .px_3()
                .py_0p5()
                .cursor_pointer()
                .when(ix == self.connection_ix, |d| d.bg(Theme::accent_dim()))
                .hover(|d| d.bg(Theme::border()))
                .on_click(
                    cx.listener(move |this, _, window, cx| this.pick_connection(ix, window, cx)),
                )
        };
        let other = list.len();
        let menu = div()
            .id("connection-list")
            .key_context("ConnectionList")
            .track_focus(&self.connection_focus)
            .absolute()
            .top(px(30.))
            .left(px(8.))
            .w(px(260.))
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
                this.close_connections(window, cx)
            }))
            .on_action(cx.listener(|this, _: &PrevDatabase, _, cx| this.move_connection(-1, cx)))
            .on_action(cx.listener(|this, _: &NextDatabase, _, cx| this.move_connection(1, cx)))
            .on_action(cx.listener(|this, _: &PickDatabase, window, cx| {
                let ix = this.connection_ix;
                this.pick_connection(ix, window, cx)
            }))
            .when(!list.is_empty(), |d| {
                d.child(
                    div()
                        .px_3()
                        .py_0p5()
                        .text_color(Theme::muted())
                        .child("Open in the browser"),
                )
            })
            .children(list.iter().enumerate().map(|(ix, (saved, database))| {
                row(SharedString::from(format!("conn-{}", saved.id)), ix)
                    .when(current.as_ref() == Some(&saved.id), |d| {
                        d.text_color(Theme::accent())
                    })
                    .child(color_dot(saved.color))
                    .child(saved.name.clone())
                    .children(
                        database
                            .clone()
                            .map(|db| div().text_color(Theme::muted()).child(db)),
                    )
            }))
            .child(
                row("conn-other".into(), other)
                    .when(!list.is_empty(), |d| {
                        d.mt_1().border_t_1().border_color(Theme::border())
                    })
                    .child("Other connection…"),
            );
        let backdrop = div()
            .id("connection-backdrop")
            .absolute()
            .size_full()
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| this.close_connections(window, cx)),
            );
        Some(div().absolute().size_full().child(backdrop).child(menu))
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
            let grid = grid.read(cx);
            let text = grid.export(format);
            let n = grid.picked_rows();
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
            let rows = if n == 1 { "row" } else { "rows" };
            self.show_toast(format!("Copied {n} {rows} as {}", format.label()), cx);
        }
    }

    fn show_toast(&mut self, text: String, cx: &mut Context<Self>) {
        let clear = cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(1800))
                .await;
            this.update(cx, |this, cx| {
                this.toast = None;
                cx.notify();
            })
            .ok();
        });
        // replacing the task drops the previous timer
        self.toast = Some((text.into(), clear));
        cx.notify();
    }

    #[cfg(test)]
    pub fn toast(&self) -> Option<&str> {
        self.toast.as_ref().map(|(t, _)| t.as_ref())
    }

    fn shown_plan(&self) -> Option<Entity<PlanView>> {
        let out = self.output.as_ref()?;
        (out.plan_ix() == Some(out.shown)).then(|| out.plan.clone())?
    }

    fn copy_plan(&mut self, cx: &mut Context<Self>) {
        if let Some(plan) = self.shown_plan() {
            let text = plan.read(cx).plan.raw.clone();
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
            self.show_toast("Copied the plan".into(), cx);
        }
    }

    fn save_plan(&mut self, cx: &mut Context<Self>) {
        let Some(plan) = self.shown_plan() else {
            return;
        };
        let plan = &plan.read(cx).plan;
        let name = format!("plan.{}", plan.format.extension());
        let text = plan.raw.clone();
        self.save_as(name, move |_| Some(text), cx);
    }

    fn export(&mut self, format: Format, cx: &mut Context<Self>) {
        let Some(grid) = self.shown_grid() else {
            return;
        };
        let name = format!("result.{}", format.extension());
        self.save_as(
            name,
            move |cx| grid.read_with(cx, |g, _| g.export_all(format)).ok(),
            cx,
        );
    }

    /// Asks where, then writes what `text` gives.
    fn save_as(
        &mut self,
        name: String,
        text: impl FnOnce(&mut gpui::AsyncApp) -> Option<String> + 'static,
        cx: &mut Context<Self>,
    ) {
        let dir = directories::UserDirs::new()
            .and_then(|d| {
                d.download_dir()
                    .map(|p| p.to_path_buf())
                    .or(Some(d.home_dir().to_path_buf()))
            })
            .unwrap_or_default();
        let path = cx.prompt_for_new_path(&dir, Some(&name));
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(path))) = path.await else {
                return;
            };
            let Some(text) = text(cx) else {
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
        let shown = out.shown.min(out.messages_ix());
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
        if let Some(ix) = out.plan_ix() {
            tabs.push(tab(ix, "Plan".into(), shown == ix));
        }
        tabs.push(tab(
            out.messages_ix(),
            "Messages".into(),
            shown == out.messages_ix(),
        ));

        let small = |id: &'static str, label: &'static str| {
            div()
                .id(id)
                .debug_selector(move || id.into())
                .px_1p5()
                .rounded_sm()
                .cursor_pointer()
                .text_color(Theme::muted())
                .hover(|d| d.bg(Theme::border()).text_color(Theme::text()))
                .child(label)
        };
        let has_grid = shown < out.sets.len();
        let has_plan = out.plan_ix() == Some(shown);
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
            .when(has_plan, |d| {
                d.child(
                    small("copy-plan", "Copy plan")
                        .on_click(cx.listener(|this, _, _, cx| this.copy_plan(cx))),
                )
                .child(
                    small("save-plan", "Save plan")
                        .on_click(cx.listener(|this, _, _, cx| this.save_plan(cx))),
                )
            })
            .child(
                small("toggle-panel", if self.panel_collapsed { "▴" } else { "▾" })
                    .ml_2()
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_panel(cx))),
            );

        let body = match (out.sets.get(shown), &out.plan) {
            (Some(grid), _) => grid.clone().into_any_element(),
            (None, Some(plan)) if has_plan => plan.clone().into_any_element(),
            _ => self.render_messages(out, cx).into_any_element(),
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
                .children(self.toast.as_ref().map(|(text, _)| {
                    div()
                        .absolute()
                        .top(PANEL_BAR + px(6.))
                        .right(px(10.))
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .border_1()
                        .border_color(Theme::accent())
                        .bg(Theme::bg())
                        .shadow_md()
                        .child(format!("✓ {text}"))
                }))
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
    pub fn apply_for_test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.apply_origin(window, cx);
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

/// The statements in a run that change data, schema or access, to agree
/// to first: a title, and each statement on a line of its own with where
/// it is.
fn change_warning(
    driver: DriverKind,
    target: &Target,
    connection: &str,
) -> Option<(String, String)> {
    const SHOWN: usize = 8;
    let found: Vec<_> = script::split(driver, &target.text)
        .into_iter()
        .filter_map(|s| Some((s.line, script::change_kind(s.text)?, s.text)))
        .collect();
    let title = match found.as_slice() {
        [] => return None,
        [(_, kind, _)] => format!("Run this {kind} on {connection}?"),
        _ => format!(
            "Run {} statements that change data on {connection}?",
            found.len()
        ),
    };
    let mut detail = Vec::new();
    for (line, kind, text) in found.iter().take(SHOWN) {
        let mut one: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if one.chars().count() > 100 {
            one = one.chars().take(99).collect::<String>() + "…";
        }
        detail.push(format!("Line {}: {one}", target.first_line + line));
        let note = match *kind {
            _ if script::unfiltered_write(text).is_some() => {
                "No WHERE: this changes every row in the table."
            }
            "TRUNCATE" => "Removes every row in the table.",
            "DROP" => "Drops it with everything in it.",
            "CALL" | "EXEC" | "EXECUTE" => "Runs code that may change anything.",
            _ => "",
        };
        if !note.is_empty() {
            detail.push(format!("    {note}"));
        }
    }
    if found.len() > SHOWN {
        detail.push(format!("…and {} more.", found.len() - SHOWN));
    }
    Some((title, detail.join("\n")))
}

/// What Explain Analyze would do that the user should agree to first: the
/// title, the detail and the label of the button that runs it.
fn analyze_warning(
    sql: &str,
    connection: &str,
    red: bool,
) -> Option<(String, String, &'static str)> {
    let plain = "Explain without Analyze shows the plan without running anything.";
    if script::write_reason(sql).is_none() {
        return red.then(|| {
            (
                format!("Explain Analyze runs this query on {connection}."),
                format!(
                    "It takes as long and loads the server as much as running it does.\n\n{plain}"
                ),
                "Run",
            )
        });
    }
    let kind = sql
        .split_whitespace()
        .next()
        .unwrap_or("statement")
        .to_ascii_uppercase();
    let every_row = match script::unfiltered_write(sql) {
        Some(_) => " It has no WHERE, so that is every row of the table.",
        None => "",
    };
    Some((
        format!("Explain Analyze runs this {kind} on {connection} for real."),
        format!(
            "Its changes are rolled back afterwards, but until then:\n\
             • the rows it changes stay locked, and other sessions writing \
             them wait.{every_row}\n\
             • it takes as long and loads the server as much as running it does.\n\n\
             Rolling back doesn't undo everything:\n\
             • sequence values it uses aren't given back.\n\
             • anything that reaches outside the database still happens.\n\n\
             {plain}"
        ),
        "Run and Roll Back",
    ))
}

pub(crate) struct Tip(pub SharedString);

impl Render for Tip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .bg(Theme::panel())
            .border_1()
            .border_color(Theme::border())
            .rounded_sm()
            .text_size(px(11.5))
            .text_color(Theme::text())
            .child(self.0.clone())
    }
}

const DEFINER_HINT: &str = "The DEFINER clause names another user, which takes extra \
                            privileges. Remove it to create the routine as yourself.";

/// MySQL's "you need SUPER or SET_USER_ID" for a DEFINER naming someone else.
fn definer_refused(e: &DbError) -> bool {
    matches!(e, DbError::Query { code: Some(c), .. } if c == "1227")
}

async fn run_to_end(session: &honcho_db::Session, sql: String) -> Result<(), DbError> {
    let mut handle = session.execute(sql);
    let mut result = Ok(());
    while let Some(event) = handle.next().await {
        if let QueryEvent::Finished(r) = event {
            result = r.map(|_| ());
        }
    }
    result
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
        if std::mem::take(&mut self.focus_connections) {
            window.focus(&self.connection_focus);
        }
        // a Run or Explain that was waiting, now there's a connection
        if self.connection.is_some() && !self.reconnecting {
            if let Some((whole, mode)) = self.pending.take() {
                cx.defer_in(window, move |this, window, cx| {
                    this.start_action(whole, mode, window, cx)
                });
            }
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
            .on_action(
                cx.listener(|this, _: &Explain, window, cx| this.explain_action(false, window, cx)),
            )
            .on_action(cx.listener(|this, _: &ExplainAnalyze, window, cx| {
                this.explain_action(true, window, cx)
            }))
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
            .children(self.render_connections(cx))
    }
}

/// Drawn rather than "▶" because the glyph's height and baseline change with
/// the font, which left it off centre next to the label.
fn play_icon() -> impl IntoElement {
    canvas(
        |_, _, _| {},
        |b, _, window, _| {
            let mut path = PathBuilder::fill();
            path.move_to(point(b.left(), b.top()));
            path.line_to(point(b.right(), b.top() + b.size.height / 2.));
            path.line_to(point(b.left(), b.bottom()));
            path.close();
            if let Ok(path) = path.build() {
                window.paint_path(path, Theme::text());
            }
        },
    )
    .w(px(7.))
    .h(px(8.))
    .flex_none()
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
    fn analyze_asks_about_writes_and_about_reads_on_red_connections() {
        let (title, detail, run) = analyze_warning("delete from t", "prod", false).unwrap();
        assert_eq!(title, "Explain Analyze runs this DELETE on prod for real.");
        assert!(detail.contains("every row"));
        assert!(detail.contains("sequence"));
        assert_eq!(run, "Run and Roll Back");
        let (_, detail, _) =
            analyze_warning("update t set a = 1 where id = 2", "prod", false).unwrap();
        assert!(!detail.contains("every row"));
        assert!(analyze_warning("select 1", "dev", false).is_none());
        let (title, _, run) = analyze_warning("select 1", "prod", true).unwrap();
        assert_eq!(title, "Explain Analyze runs this query on prod.");
        assert_eq!(run, "Run");
    }

    #[test]
    fn change_warning_lists_each_change() {
        let target = |text: &str| Target {
            text: text.into(),
            range: 0..0,
            first_line: 9,
            mode: RunMode::Execute,
        };
        let w = |text: &str| change_warning(DriverKind::MySql, &target(text), "prod");
        assert_eq!(w("select 1;\ncreate table t (x int)"), None);

        let (title, detail) = w("select 1;\nupdate t\n  set a = 1\n  where id = 2").unwrap();
        assert_eq!(title, "Run this UPDATE on prod?");
        assert_eq!(detail, "Line 11: update t set a = 1 where id = 2");

        let (title, detail) =
            w("delete from logs;\nselect 1;\nupdate t set a = 1 where id = 2").unwrap();
        assert_eq!(title, "Run 2 statements that change data on prod?");
        assert_eq!(
            detail,
            "Line 10: delete from logs\n    No WHERE: this changes every row in the table.\n\
             Line 12: update t set a = 1 where id = 2"
        );

        let (title, detail) = w("insert into t values (1);\ntruncate logs;\ndrop table t").unwrap();
        assert_eq!(title, "Run 3 statements that change data on prod?");
        assert_eq!(
            detail,
            "Line 10: insert into t values (1)\n\
             Line 11: truncate logs\n    Removes every row in the table.\n\
             Line 12: drop table t\n    Drops it with everything in it."
        );
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
