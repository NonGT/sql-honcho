//! The object browser: saved connections down to columns and indexes,
//! loaded as nodes open. Each connection gets its own session, so a long
//! query in a tab doesn't hold up the tree.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use gpui::{
    actions, anchored, deferred, div, prelude::*, px, uniform_list, App, ClipboardItem, Context,
    Entity, EventEmitter, FocusHandle, Focusable, MouseButton, MouseDownEvent, Pixels, Point, Task,
    UniformListScrollHandle, Window,
};
use honcho_config::SavedConnection;
use honcho_db::{
    ColumnInfo, DbError, DriverKind, IndexInfo, ObjectKind, ObjectRef, RoutineParam, Scope, Session,
};

use crate::connection_dialog::color_dot;
use crate::connections::{self, Connections};
use crate::editor_view::{self, EditorView};
use crate::theme::{self, Theme};
use honcho_db::routine::{self, ident, qualified};

actions!(browser, [ToggleBrowser]);

const ROW_HEIGHT: Pixels = px(22.);
pub const DEFAULT_WIDTH: Pixels = px(260.);
const INDENT: f32 = 14.;
const SEARCH_LIMIT: usize = 500;
const SEARCH_DELAY: Duration = Duration::from_millis(250);
const GROUPS: [ObjectKind; 6] = [
    ObjectKind::Table,
    ObjectKind::View,
    ObjectKind::Procedure,
    ObjectKind::Function,
    ObjectKind::Trigger,
    ObjectKind::Sequence,
];

/// A node's place in the tree, one segment per level, e.g.
/// `["c:<id>", "d:app", "g:Table", "o:users", "columns"]`. Kept as strings
/// so expanded nodes can be saved.
type Path = Vec<String>;

pub enum BrowserEvent {
    /// Open a tab with `text` on this connection, and run it when `run`.
    /// `origin` is the object the text recreates, with its definition then.
    Open {
        saved: Box<SavedConnection>,
        database: Option<String>,
        text: String,
        run: bool,
        origin: Option<Box<(ObjectRef, String)>>,
    },
    /// Ask for a routine's parameter values and run it.
    RunRoutine {
        saved: Box<SavedConnection>,
        object: ObjectRef,
        params: Vec<RoutineParam>,
    },
    /// Put text into the current editor.
    Insert(String),
    /// Write this database's routines and views into a folder.
    SaveProject {
        saved: Box<SavedConnection>,
        database: String,
    },
}

#[derive(Clone, Debug)]
enum Item {
    Connection(Box<SavedConnection>),
    Database(String),
    Group(String, ObjectKind, usize),
    Object(ObjectRef),
    Columns(ObjectRef),
    Column(ColumnInfo),
    Indexes(ObjectRef),
    Index(IndexInfo),
}

impl Item {
    fn segment(&self) -> String {
        match self {
            Item::Connection(c) => format!("c:{}", c.id),
            Item::Database(d) => format!("d:{d}"),
            Item::Group(_, k, _) => format!("g:{k:?}"),
            Item::Object(o) => format!("o:{}", o.name),
            Item::Columns(_) => "columns".into(),
            Item::Column(c) => format!("col:{}", c.name),
            Item::Indexes(_) => "indexes".into(),
            Item::Index(i) => format!("idx:{}", i.name),
        }
    }

    fn expandable(&self) -> bool {
        match self {
            Item::Object(o) => matches!(o.kind, ObjectKind::Table | ObjectKind::View),
            Item::Column(_) | Item::Index(_) => false,
            _ => true,
        }
    }

    fn label(&self) -> String {
        match self {
            Item::Connection(c) => c.name.clone(),
            Item::Database(d) => d.clone(),
            Item::Group(_, k, n) => format!("{} ({n})", group_name(*k)),
            Item::Object(o) => o.name.clone(),
            Item::Columns(_) => "Columns".into(),
            Item::Column(c) => c.name.clone(),
            Item::Indexes(_) => "Indexes".into(),
            Item::Index(i) => i.name.clone(),
        }
    }

    fn detail(&self) -> Option<String> {
        match self {
            Item::Column(c) => Some(format!(
                "{}{}{}",
                c.data_type,
                if c.nullable { "" } else { " not null" },
                if c.primary_key { " · PK" } else { "" }
            )),
            Item::Index(i) => Some(format!(
                "{}{}",
                i.columns.join(", "),
                if i.primary {
                    " · primary"
                } else if i.unique {
                    " · unique"
                } else {
                    ""
                }
            )),
            _ => None,
        }
    }

    fn icon(&self) -> &'static str {
        match self {
            Item::Connection(_) => "",
            Item::Database(_) => "▤",
            Item::Group(..) | Item::Columns(_) | Item::Indexes(_) => "≡",
            Item::Object(o) => match o.kind {
                ObjectKind::Table => "▦",
                ObjectKind::View | ObjectKind::MaterializedView => "◫",
                ObjectKind::Procedure | ObjectKind::Function => "ƒ",
                ObjectKind::Trigger => "⚡",
                ObjectKind::Sequence => "#",
            },
            Item::Column(c) if c.primary_key => "⚷",
            Item::Column(_) => "·",
            Item::Index(_) => "⇅",
        }
    }
}

fn group_name(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Table => "Tables",
        ObjectKind::View => "Views",
        ObjectKind::MaterializedView => "Materialized views",
        ObjectKind::Procedure => "Procedures",
        ObjectKind::Function => "Functions",
        ObjectKind::Trigger => "Triggers",
        ObjectKind::Sequence => "Sequences",
    }
}

enum Children {
    Loading,
    Loaded(Vec<Item>),
    Failed(String),
}

enum Conn {
    Opening,
    Open(Session),
    Failed(String),
}

/// One visible line of the tree.
#[derive(Clone)]
struct Row {
    path: Path,
    item: Item,
    depth: usize,
    expanded: bool,
    /// Loading or failed, shown under an expanded node.
    note: Option<(String, bool)>,
}

struct Menu {
    path: Path,
    item: Item,
    position: Point<Pixels>,
}

struct Search {
    term: String,
    /// Per connection id, whatever matched.
    results: HashMap<String, Vec<ObjectRef>>,
    _task: Task<()>,
}

pub struct ObjectBrowser {
    focus: FocusHandle,
    filter: Entity<EditorView>,
    conns: HashMap<String, Conn>,
    children: HashMap<Path, Children>,
    expanded: HashSet<Path>,
    selected: Option<Path>,
    rows: Vec<Row>,
    menu: Option<Menu>,
    search: Option<Search>,
    scroll: UniformListScrollHandle,
    /// Connections reopened after dropping and not loaded anything since;
    /// a second drop is reported instead of retried.
    reconnected: HashSet<String>,
    /// Actions waiting for a connection's session to (re)open.
    pending: HashMap<String, Vec<(Path, Action)>>,
    /// The pane's width, set by dragging its edge.
    pub width: Pixels,
}

impl EventEmitter<BrowserEvent> for ObjectBrowser {}

impl ObjectBrowser {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let filter = cx.new(|cx| EditorView::single_line("BrowserFilter", "Filter", cx));
        cx.observe(&filter, |this, _, cx| this.filter_changed(cx))
            .detach();
        let mut this = Self {
            focus: cx.focus_handle(),
            filter,
            conns: HashMap::new(),
            children: HashMap::new(),
            expanded: HashSet::new(),
            selected: None,
            rows: Vec::new(),
            menu: None,
            search: None,
            scroll: UniformListScrollHandle::new(),
            reconnected: HashSet::new(),
            pending: HashMap::new(),
            width: DEFAULT_WIDTH,
        };
        this.rebuild(cx);
        this
    }

    fn saved(&self, id: &str, cx: &App) -> Option<SavedConnection> {
        cx.global::<Connections>().store.get(id).cloned()
    }

    fn session(&self, id: &str) -> Option<Session> {
        match self.conns.get(id) {
            Some(Conn::Open(s)) => Some(s.clone()),
            _ => None,
        }
    }

    /// Children of a node: from a search while filtering, otherwise from
    /// what's been loaded.
    fn children_of(&self, path: &Path, item: &Item, cx: &App) -> Option<Result<Vec<Item>, String>> {
        if let (Some(search), true) = (&self.search, path.len() <= 3) {
            if let Some(items) = search_children(search, path, item, cx) {
                return Some(Ok(items));
            }
        }
        match self.children.get(path)? {
            Children::Loading => None,
            Children::Loaded(items) => Some(Ok(items.clone())),
            Children::Failed(e) => Some(Err(e.clone())),
        }
    }

    /// Flattens the expanded part of the tree into rows.
    fn rebuild(&mut self, cx: &mut Context<Self>) {
        let mut rows = Vec::new();
        let filtering = self.search.is_some();
        let term = self
            .search
            .as_ref()
            .map(|s| s.term.to_lowercase())
            .unwrap_or_default();
        for saved in cx.global::<Connections>().store.connections() {
            let item = Item::Connection(Box::new(saved.clone()));
            let path = vec![item.segment()];
            // while filtering, connections that can't be searched show only
            // when their name matches
            if filtering
                && self.session(&saved.id).is_none()
                && !saved.name.to_lowercase().contains(&term)
            {
                continue;
            }
            self.walk(path, item, 0, &mut rows, cx);
        }
        self.rows = rows;
        cx.notify();
    }

    fn walk(&self, path: Path, item: Item, depth: usize, rows: &mut Vec<Row>, cx: &App) {
        // while filtering, everything down to the matches is open
        let expanded = self.expanded.contains(&path)
            || (self.search.is_some()
                && path.len() <= 3
                && matches!(
                    item,
                    Item::Connection(_) | Item::Database(_) | Item::Group(..)
                )
                && self
                    .search
                    .as_ref()
                    .is_some_and(|s| search_children(s, &path, &item, cx).is_some()));
        let children = expanded.then(|| self.children_of(&path, &item, cx));
        let note = match (&children, &item) {
            (Some(None), Item::Connection(c)) => match self.conns.get(&c.id) {
                Some(Conn::Failed(e)) => Some((e.clone(), true)),
                _ => Some(("Loading…".into(), false)),
            },
            (Some(None), _) => Some(("Loading…".into(), false)),
            (Some(Some(Err(e))), _) => Some((e.clone(), true)),
            (Some(Some(Ok(items))), _) if items.is_empty() => Some(("Empty".into(), false)),
            _ => None,
        };
        rows.push(Row {
            path: path.clone(),
            item,
            depth,
            expanded,
            note,
        });
        if let Some(Some(Ok(items))) = children {
            for child in items {
                let mut p = path.clone();
                p.push(child.segment());
                self.walk(p, child, depth + 1, rows, cx);
            }
        }
    }

    fn toggle(&mut self, path: Path, item: Item, cx: &mut Context<Self>) {
        if !item.expandable() {
            return;
        }
        if !self.expanded.remove(&path) {
            self.expanded.insert(path.clone());
            self.load(path, item, false, cx);
        }
        self.save_expanded(cx);
        self.rebuild(cx);
    }

    /// Loads a node's children unless they're cached; `force` refetches.
    fn load(&mut self, path: Path, item: Item, force: bool, cx: &mut Context<Self>) {
        if !force && self.children.contains_key(&path) {
            return;
        }
        let conn_id = path[0].trim_start_matches("c:").to_string();
        if let Item::Connection(saved) = &item {
            if force || !matches!(self.conns.get(&conn_id), Some(Conn::Open(_))) {
                self.children.remove(&path);
                self.open_connection(*saved.clone(), cx);
                return;
            }
        }
        let Some(session) = self.session(&conn_id) else {
            return;
        };
        self.children.insert(path.clone(), Children::Loading);
        cx.spawn(async move |this, cx| {
            let loaded = fetch(&session, &item).await;
            this.update(cx, |this, cx| {
                if loaded.is_ok() {
                    this.reconnected.remove(&conn_id);
                }
                match loaded {
                    Ok(Fetched::Items(items)) => {
                        this.children.insert(path.clone(), Children::Loaded(items));
                    }
                    Ok(Fetched::Database(groups)) => {
                        // one query fills the database and every group under it
                        let db = path.clone();
                        let mut items = Vec::new();
                        for (group, objects) in groups {
                            let mut p = db.clone();
                            p.push(group.segment());
                            items.push(group);
                            this.children.insert(p, Children::Loaded(objects));
                        }
                        this.children.insert(db, Children::Loaded(items));
                    }
                    Err(e) if e.is_fatal() && this.reconnect(&conn_id, cx) => {
                        // the reopened session reloads every open node,
                        // this one included
                        return;
                    }
                    Err(e) if e.is_fatal() => {
                        this.children.insert(
                            path.clone(),
                            Children::Failed(format!(
                                "Connection lost: {e}. Refresh to try again."
                            )),
                        );
                    }
                    Err(e) => {
                        this.children
                            .insert(path.clone(), Children::Failed(e.to_string()));
                    }
                }
                this.load_open_children(&path, cx);
                this.rebuild(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Children that were left expanded load as soon as their parent has,
    /// which brings a remembered subtree back one level at a time.
    fn load_open_children(&mut self, parent: &Path, cx: &mut Context<Self>) {
        let Some(Children::Loaded(items)) = self.children.get(parent) else {
            return;
        };
        let open: Vec<(Path, Item)> = items
            .iter()
            .map(|item| {
                let mut p = parent.clone();
                p.push(item.segment());
                (p, item.clone())
            })
            .filter(|(p, _)| self.expanded.contains(p))
            .collect();
        for (path, item) in open {
            if self.children.contains_key(&path) {
                // a database fills its groups itself; go one further
                self.load_open_children(&path, cx);
            } else {
                self.load(path, item, false, cx);
            }
        }
    }

    /// Reopens a connection whose session dropped, once. False when it was
    /// just reopened and dropped again, so the caller reports it.
    fn reconnect(&mut self, id: &str, cx: &mut Context<Self>) -> bool {
        let Some(saved) = self.saved(id, cx) else {
            return false;
        };
        // other loads cut off by the same drop wait for the same reconnect
        if matches!(self.conns.get(id), Some(Conn::Opening)) {
            return true;
        }
        if !self.reconnected.insert(id.to_string()) {
            return false;
        }
        // what's loaded is still right; only the session is new
        self.open_session(saved, true, cx);
        true
    }

    /// Opens (or reopens) a connection and loads its databases and every
    /// open node under it.
    fn open_connection(&mut self, saved: SavedConnection, cx: &mut Context<Self>) {
        self.open_session(saved, false, cx);
    }

    /// With `keep`, the tree stays as it is and only the nodes that were
    /// loading when the session dropped load again; otherwise everything
    /// under the connection is fetched afresh.
    fn open_session(&mut self, saved: SavedConnection, keep: bool, cx: &mut Context<Self>) {
        let id = saved.id.clone();
        if let Some(Conn::Open(old)) = self.conns.remove(&id) {
            cx.background_spawn(async move { old.close().await })
                .detach();
        }
        self.conns.insert(id.clone(), Conn::Opening);
        if !keep {
            self.children.retain(|p, _| p[0] != format!("c:{id}"));
        }
        let opening = connections::open_session(saved.clone(), None, cx);
        cx.spawn(async move |this, cx| {
            let result = opening.await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(session) => {
                        this.conns.insert(id.clone(), Conn::Open(session));
                        for (path, action) in this.pending.remove(&id).unwrap_or_default() {
                            this.perform(path, action, true, cx);
                        }
                        if keep {
                            for (path, item) in this.cut_off(&id, cx) {
                                this.children.remove(&path);
                                this.load(path, item, false, cx);
                            }
                        } else {
                            // the databases, then whatever was open under them
                            let root = vec![format!("c:{id}")];
                            if let Some(item) = this.item_at(&root, cx) {
                                this.load(root, item, false, cx);
                            }
                        }
                        if let Some(search) = &this.search {
                            let term = search.term.clone();
                            this.start_search(term, cx);
                        }
                    }
                    Err(DbError::HostKey(_)) => {
                        this.pending.remove(&id);
                        this.conns.insert(
                            id,
                            Conn::Failed(
                                "Unknown SSH host key. Connect once from the Connections dialog to trust it."
                                    .into(),
                            ),
                        );
                    }
                    Err(e) => {
                        let message = if this.reconnected.contains(&id) {
                            format!("Connection lost and reconnecting failed: {e}")
                        } else {
                            e.to_string()
                        };
                        for (path, action) in this.pending.remove(&id).unwrap_or_default() {
                            if let (Action::Definition(o), Some(saved)) =
                                (action, this.conn_of(&path, cx))
                            {
                                cx.emit(BrowserEvent::Open {
                                    saved: Box::new(saved),
                                    database: o.database.clone(),
                                    text: format!("-- couldn't connect to open {}: {message}\n", o.name),
                                    run: false,
                                    origin: None,
                                });
                            }
                        }
                        // the tree stays; the nodes that were waiting say why
                        for (path, _) in this.cut_off(&id, cx) {
                            this.children
                                .insert(path, Children::Failed(message.clone()));
                        }
                        this.conns.insert(id, Conn::Failed(message));
                    }
                }
                this.rebuild(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Nodes of a connection left loading when its session dropped.
    fn cut_off(&self, id: &str, cx: &App) -> Vec<(Path, Item)> {
        let root = format!("c:{id}");
        self.children
            .iter()
            .filter(|(p, c)| p[0] == root && matches!(c, Children::Loading))
            .filter_map(|(p, _)| Some((p.clone(), self.item_at(p, cx)?)))
            .collect()
    }

    /// The item at a path, if the tree has loaded down to it.
    fn item_at(&self, path: &Path, cx: &App) -> Option<Item> {
        let id = path.first()?.strip_prefix("c:")?;
        let mut item = Item::Connection(Box::new(self.saved(id, cx)?));
        for depth in 1..path.len() {
            let parent = &path[..depth];
            let Some(Children::Loaded(items)) = self.children.get(parent) else {
                return None;
            };
            item = items.iter().find(|i| i.segment() == path[depth])?.clone();
        }
        Some(item)
    }

    fn filter_changed(&mut self, cx: &mut Context<Self>) {
        let term = self.filter.read(cx).text().trim().to_string();
        if term.is_empty() {
            if self.search.take().is_some() {
                self.rebuild(cx);
            }
            return;
        }
        if self.search.as_ref().is_some_and(|s| s.term == term) {
            return;
        }
        self.start_search(term, cx);
    }

    /// Asks every open connection for matching names, after a pause in
    /// typing.
    fn start_search(&mut self, term: String, cx: &mut Context<Self>) {
        let sessions: Vec<(String, Session)> = self
            .conns
            .iter()
            .filter_map(|(id, c)| match c {
                Conn::Open(s) => Some((id.clone(), s.clone())),
                _ => None,
            })
            .collect();
        let query = term.clone();
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SEARCH_DELAY).await;
            let mut results = HashMap::new();
            let mut dropped = Vec::new();
            for (id, session) in sessions {
                // `body:term` looks inside routine code instead of at names
                let found = match query.strip_prefix("body:") {
                    Some(term) => session.search_bodies(term.trim(), SEARCH_LIMIT).await,
                    None => session.search(query.clone(), SEARCH_LIMIT).await,
                };
                match found {
                    Ok(found) => {
                        results.insert(id, found);
                    }
                    Err(e) if e.is_fatal() => dropped.push(id),
                    Err(_) => {}
                }
            }
            this.update(cx, |this, cx| {
                if let Some(search) = &mut this.search {
                    if search.term == query {
                        search.results = results;
                    }
                }
                // reopening searches again once the session is back
                for id in dropped {
                    if !this.reconnect(&id, cx) {
                        this.conns.insert(
                            id,
                            Conn::Failed("Connection lost and reconnecting failed.".into()),
                        );
                    }
                }
                this.rebuild(cx);
            })
            .ok();
        });
        // keep showing the last results until the new ones arrive
        let results = self.search.take().map(|s| s.results).unwrap_or_default();
        self.search = Some(Search {
            term,
            results,
            _task: task,
        });
        self.rebuild(cx);
    }

    fn select(
        &mut self,
        ix: usize,
        e: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(row) = self.rows.get(ix).cloned() else {
            return;
        };
        window.focus(&self.focus);
        self.selected = Some(row.path.clone());
        self.menu = None;
        if e.button == MouseButton::Right {
            self.menu = Some(Menu {
                path: row.path,
                item: row.item,
                position: e.position,
            });
        } else if e.click_count == 2 && !row.item.expandable() {
            self.default_action(&row.path, &row.item, cx);
        } else if e.click_count == 2 && matches!(row.item, Item::Object(_)) {
            // double-clicking a table opens its rows, not its columns
            self.default_action(&row.path, &row.item, cx);
        } else {
            self.toggle(row.path, row.item, cx);
        }
        cx.notify();
    }

    fn default_action(&mut self, path: &Path, item: &Item, cx: &mut Context<Self>) {
        match item {
            Item::Object(o) if matches!(o.kind, ObjectKind::Table | ObjectKind::View) => {
                self.select_top(path, o, cx)
            }
            Item::Object(o) => self.open_definition(path, o, cx),
            Item::Column(c) => cx.emit(BrowserEvent::Insert(ident(DriverKind::MySql, &c.name))),
            _ => {}
        }
    }

    fn conn_of(&self, path: &Path, cx: &App) -> Option<SavedConnection> {
        self.saved(path.first()?.strip_prefix("c:")?, cx)
    }

    fn select_top(&mut self, path: &Path, object: &ObjectRef, cx: &mut Context<Self>) {
        let Some(saved) = self.conn_of(path, cx) else {
            return;
        };
        let text = format!(
            "SELECT * FROM {} LIMIT 1000;",
            qualified(saved.driver, object)
        );
        cx.emit(BrowserEvent::Open {
            saved: Box::new(saved),
            database: object.database.clone(),
            text,
            run: true,
            origin: None,
        });
    }

    /// Asks for the routine's parameters, then for their values.
    fn run_routine(&mut self, path: &Path, object: &ObjectRef, cx: &mut Context<Self>) {
        self.perform(path.clone(), Action::Run(object.clone()), false, cx);
    }

    /// Fetches a database's objects again after something was applied,
    /// leaving the tree as it is until the new list arrives.
    pub fn reload_database(&mut self, conn_id: &str, database: &str, cx: &mut Context<Self>) {
        let path = vec![format!("c:{conn_id}"), format!("d:{database}")];
        let (Some(session), Some(Children::Loaded(_))) =
            (self.session(conn_id), self.children.get(&path))
        else {
            return;
        };
        let item = Item::Database(database.to_string());
        cx.spawn(async move |this, cx| {
            let Ok(Fetched::Database(groups)) = fetch(&session, &item).await else {
                return;
            };
            this.update(cx, |this, cx| {
                let mut items = Vec::new();
                for (group, objects) in groups {
                    let mut p = path.clone();
                    p.push(group.segment());
                    items.push(group);
                    this.children.insert(p, Children::Loaded(objects));
                }
                this.children.insert(path, Children::Loaded(items));
                this.rebuild(cx);
            })
            .ok();
        })
        .detach();
    }

    fn open_definition(&mut self, path: &Path, object: &ObjectRef, cx: &mut Context<Self>) {
        self.perform(path.clone(), Action::Definition(object.clone()), false, cx);
    }

    /// Runs an action with the connection's session, opening it first when
    /// it isn't open (failed earlier, or not opened yet). A session that
    /// turns out to have dropped is reopened and the action tried once more.
    fn perform(&mut self, path: Path, action: Action, retried: bool, cx: &mut Context<Self>) {
        let Some(saved) = self.conn_of(&path, cx) else {
            return;
        };
        let id = saved.id.clone();
        let session = match self.conns.get(&id) {
            Some(Conn::Open(s)) => s.clone(),
            Some(Conn::Opening) => {
                self.pending.entry(id).or_default().push((path, action));
                return;
            }
            _ => {
                self.pending.entry(id).or_default().push((path, action));
                self.open_session(saved, true, cx);
                self.rebuild(cx);
                return;
            }
        };
        cx.spawn(async move |this, cx| {
            let result = match &action {
                Action::Definition(o) => session.definition(o.clone()).await.map(Done::Definition),
                Action::Run(o) => session.parameters(o.clone()).await.map(Done::Params),
                Action::Template(o, _) => session.columns(o.clone()).await.map(Done::Columns),
            };
            this.update(cx, |this, cx| {
                if let Err(e) = &result {
                    if e.is_fatal() && !retried {
                        let saved = saved.clone();
                        this.pending
                            .entry(saved.id.clone())
                            .or_default()
                            .push((path, action));
                        if !matches!(this.conns.get(&saved.id), Some(Conn::Opening)) {
                            this.open_session(saved, true, cx);
                        }
                        return;
                    }
                }
                match (action, result) {
                    (Action::Definition(o), result) => {
                        let (text, origin) = match result {
                            Ok(Done::Definition(def)) => (
                                routine::script(saved.driver, &o, &def),
                                routine::editable(&o).then(|| Box::new((o.clone(), def))),
                            ),
                            Err(e) => (
                                format!("-- couldn't read the definition of {}: {e}\n", o.name),
                                None,
                            ),
                            Ok(_) => return,
                        };
                        cx.emit(BrowserEvent::Open {
                            saved: Box::new(saved),
                            database: o.database.clone(),
                            text,
                            run: false,
                            origin,
                        });
                    }
                    (Action::Run(object), result) => {
                        let params = match result {
                            Ok(Done::Params(p)) => p,
                            _ => Vec::new(),
                        };
                        cx.emit(BrowserEvent::RunRoutine {
                            saved: Box::new(saved),
                            object,
                            params,
                        });
                    }
                    (Action::Template(o, which), Ok(Done::Columns(columns))) => {
                        cx.emit(BrowserEvent::Insert(template(
                            saved.driver,
                            &o,
                            &columns,
                            which,
                        )));
                    }
                    _ => {}
                }
            })
            .ok();
        })
        .detach();
    }

    /// SELECT / INSERT / UPDATE for a table, into the current editor.
    fn template(
        &mut self,
        path: &Path,
        object: &ObjectRef,
        which: Template,
        cx: &mut Context<Self>,
    ) {
        self.perform(
            path.clone(),
            Action::Template(object.clone(), which),
            false,
            cx,
        );
    }

    fn refresh(&mut self, path: Path, item: Item, cx: &mut Context<Self>) {
        if let Some(id) = path.first().and_then(|c| c.strip_prefix("c:")) {
            self.reconnected.remove(id);
        }
        // drop everything cached below this node
        self.children
            .retain(|p, _| !(p.len() >= path.len() && p[..path.len()] == path[..]));
        self.expanded.insert(path.clone());
        self.load(path, item, true, cx);
        self.rebuild(cx);
    }

    fn menu_action(&mut self, action: MenuAction, cx: &mut Context<Self>) {
        let Some(menu) = self.menu.take() else {
            return;
        };
        let (path, item) = (menu.path, menu.item);
        let kind = self
            .conn_of(&path, cx)
            .map(|s| s.driver)
            .unwrap_or(DriverKind::MySql);
        match (action, &item) {
            (MenuAction::Refresh, _) => self.refresh(path, item.clone(), cx),
            (MenuAction::SaveProject, Item::Database(d)) => {
                if let Some(saved) = self.conn_of(&path, cx) {
                    cx.emit(BrowserEvent::SaveProject {
                        saved: Box::new(saved),
                        database: d.clone(),
                    });
                }
            }
            (MenuAction::SelectTop, Item::Object(o)) => self.select_top(&path, o, cx),
            (MenuAction::Definition, Item::Object(o)) => self.open_definition(&path, o, cx),
            (MenuAction::Run, Item::Object(o)) => self.run_routine(&path, o, cx),
            (MenuAction::Template(t), Item::Object(o)) => self.template(&path, o, t, cx),
            (MenuAction::CopyName, Item::Object(o)) => {
                cx.write_to_clipboard(ClipboardItem::new_string(qualified(kind, o)))
            }
            (MenuAction::CopyName, other) => {
                cx.write_to_clipboard(ClipboardItem::new_string(other.label()))
            }
            (MenuAction::NewQuery, _) => {
                if let Some(saved) = self.conn_of(&path, cx) {
                    let database = match &item {
                        Item::Database(d) => Some(d.clone()),
                        _ => None,
                    };
                    cx.emit(BrowserEvent::Open {
                        saved: Box::new(saved),
                        database,
                        text: String::new(),
                        run: false,
                        origin: None,
                    });
                }
            }
            _ => {}
        }
        cx.notify();
    }

    /// Remembers the width once a drag is over.
    pub fn set_width(&mut self, width: Pixels, cx: &mut Context<Self>) {
        self.width = width;
        self.save_expanded(cx);
        cx.notify();
    }

    fn save_expanded(&self, cx: &App) {
        let path = cx
            .global::<Connections>()
            .store
            .path()
            .with_file_name("browser.json");
        let expanded: Vec<&Path> = self.expanded.iter().collect();
        let state = serde_json::json!({ "expanded": expanded, "width": f32::from(self.width) });
        let Ok(json) = serde_json::to_string(&state) else {
            return;
        };
        cx.background_spawn(async move {
            let _ = std::fs::write(path, json);
        })
        .detach();
    }

    /// Restores the nodes that were open last time. Connections stay
    /// closed until opened, which brings their subtree back.
    pub fn restore_expanded(&mut self, cx: &mut Context<Self>) {
        let path = cx
            .global::<Connections>()
            .store
            .path()
            .with_file_name("browser.json");
        let state = std::fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok());
        let Some(state) = state else {
            self.rebuild(cx);
            return;
        };
        if let Ok(paths) = serde_json::from_value::<Vec<Path>>(state["expanded"].clone()) {
            self.expanded = paths.into_iter().filter(|p| p.len() > 1).collect();
        }
        if let Some(w) = state["width"].as_f64() {
            self.width = px(w as f32);
        }
        self.rebuild(cx);
    }

    #[cfg(test)]
    pub fn labels(&self) -> Vec<String> {
        self.rows
            .iter()
            .map(|r| format!("{}{}", "  ".repeat(r.depth), r.item.label()))
            .collect()
    }

    #[cfg(test)]
    pub fn click(&mut self, label: &str, double: bool, cx: &mut Context<Self>) {
        let row = self
            .rows
            .iter()
            .find(|r| r.item.label() == label)
            .cloned()
            .unwrap_or_else(|| panic!("no row {label}"));
        if double {
            self.default_action(&row.path, &row.item, cx);
        } else {
            self.toggle(row.path, row.item, cx);
        }
    }

    /// A right-click action on the row with this label: "definition" or "run".
    #[cfg(test)]
    pub fn act(&mut self, label: &str, what: &str, cx: &mut Context<Self>) {
        let row = self
            .rows
            .iter()
            .find(|r| r.item.label() == label)
            .cloned()
            .unwrap_or_else(|| panic!("no row {label}"));
        let Item::Object(o) = &row.item else {
            panic!("{label} isn't an object");
        };
        match what {
            "definition" => self.open_definition(&row.path, o, cx),
            "run" => self.run_routine(&row.path, o, cx),
            other => panic!("no action {other}"),
        }
    }

    #[cfg(test)]
    pub fn template_for(&mut self, label: &str, which: Template, cx: &mut Context<Self>) {
        let row = self
            .rows
            .iter()
            .find(|r| r.item.label() == label)
            .cloned()
            .unwrap();
        if let Item::Object(o) = &row.item {
            self.template(&row.path, o, which, cx);
        }
    }

    #[cfg(test)]
    pub fn set_filter(&mut self, text: &str, cx: &mut Context<Self>) {
        self.filter.update(cx, |f, cx| f.set_text(text, cx));
    }
}

/// While filtering, the levels above the matches come from the search
/// results rather than from loaded nodes.
fn search_children(search: &Search, path: &Path, item: &Item, cx: &App) -> Option<Vec<Item>> {
    let id = path.first()?.strip_prefix("c:")?;
    let found = search.results.get(id)?;
    cx.global::<Connections>().store.get(id)?;
    match item {
        Item::Connection(_) => {
            let mut dbs: Vec<String> = found.iter().filter_map(|o| o.database.clone()).collect();
            dbs.dedup();
            Some(dbs.into_iter().map(Item::Database).collect())
        }
        Item::Database(db) => Some(
            GROUPS
                .iter()
                .filter_map(|&k| {
                    let n = found
                        .iter()
                        .filter(|o| o.kind == k && o.database.as_ref() == Some(db))
                        .count();
                    (n > 0).then(|| Item::Group(db.clone(), k, n))
                })
                .collect(),
        ),
        Item::Group(db, k, _) => Some(
            found
                .iter()
                .filter(|o| o.kind == *k && o.database.as_ref() == Some(db))
                .cloned()
                .map(Item::Object)
                .collect(),
        ),
        _ => None,
    }
}

enum Fetched {
    Items(Vec<Item>),
    /// A database's groups, each with its objects.
    Database(Vec<(Item, Vec<Item>)>),
}

async fn fetch(session: &Session, item: &Item) -> Result<Fetched, DbError> {
    Ok(match item {
        Item::Connection(_) => Fetched::Items(
            session
                .databases()
                .await?
                .into_iter()
                .map(Item::Database)
                .collect(),
        ),
        Item::Database(db) => {
            let objects = session
                .objects(
                    Scope {
                        database: Some(db.clone()),
                        schema: None,
                    },
                    vec![],
                )
                .await?;
            Fetched::Database(
                GROUPS
                    .iter()
                    .filter_map(|&k| {
                        let mut of: Vec<Item> = objects
                            .iter()
                            .filter(|o| o.kind == k)
                            .cloned()
                            .map(Item::Object)
                            .collect();
                        of.sort_by_key(|i| i.label().to_lowercase());
                        (!of.is_empty()).then(|| (Item::Group(db.clone(), k, of.len()), of))
                    })
                    .collect(),
            )
        }
        Item::Object(o) => {
            let mut items = vec![Item::Columns(o.clone())];
            if o.kind == ObjectKind::Table {
                items.push(Item::Indexes(o.clone()));
            }
            Fetched::Items(items)
        }
        Item::Columns(o) => Fetched::Items(
            session
                .columns(o.clone())
                .await?
                .into_iter()
                .map(Item::Column)
                .collect(),
        ),
        Item::Indexes(o) => Fetched::Items(
            session
                .indexes(o.clone())
                .await?
                .into_iter()
                .map(Item::Index)
                .collect(),
        ),
        // groups are filled with their database
        Item::Group(..) | Item::Column(_) | Item::Index(_) => Fetched::Items(vec![]),
    })
}

enum Done {
    Definition(String),
    Params(Vec<honcho_db::RoutineParam>),
    Columns(Vec<honcho_db::ColumnInfo>),
}

/// Something done to an object that needs the connection's session.
#[derive(Clone)]
enum Action {
    Definition(ObjectRef),
    Run(ObjectRef),
    Template(ObjectRef, Template),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Template {
    Select,
    Insert,
    Update,
}

#[derive(Clone, Copy)]
enum MenuAction {
    Refresh,
    SelectTop,
    Definition,
    CopyName,
    Template(Template),
    NewQuery,
    Run,
    SaveProject,
}

fn menu_for(item: &Item) -> Vec<(&'static str, MenuAction)> {
    use MenuAction::*;
    match item {
        Item::Connection(_) => vec![("New query tab", NewQuery), ("Refresh", Refresh)],
        Item::Database(_) => vec![
            ("New query tab", NewQuery),
            ("Save to project folder…", SaveProject),
            ("Refresh", Refresh),
        ],
        Item::Object(o) if matches!(o.kind, ObjectKind::Table | ObjectKind::View) => vec![
            ("Select top 1000", SelectTop),
            ("Open definition", Definition),
            ("Copy name", CopyName),
            ("SELECT template", Template(self::Template::Select)),
            ("INSERT template", Template(self::Template::Insert)),
            ("UPDATE template", Template(self::Template::Update)),
            ("Refresh", Refresh),
        ],
        Item::Object(o) if o.kind.is_routine() => vec![
            ("Open definition", Definition),
            ("Run…", Run),
            ("Copy name", CopyName),
        ],
        Item::Object(_) => vec![("Open definition", Definition), ("Copy name", CopyName)],
        Item::Column(_) | Item::Index(_) => vec![("Copy name", CopyName)],
        _ => vec![("Refresh", Refresh)],
    }
}

fn template(kind: DriverKind, o: &ObjectRef, columns: &[ColumnInfo], which: Template) -> String {
    let table = qualified(kind, o);
    let names: Vec<String> = columns.iter().map(|c| ident(kind, &c.name)).collect();
    // NULL keeps the script valid; the comment says what goes there
    let lines = |items: Vec<(String, String)>| {
        let last = items.len().saturating_sub(1);
        items
            .into_iter()
            .enumerate()
            .map(|(i, (code, note))| format!("{code}{} -- {note}", if i < last { "," } else { "" }))
            .collect::<Vec<_>>()
    };
    match which {
        Template::Select => format!("SELECT {}\nFROM {table}\nLIMIT 100;\n", names.join(", ")),
        Template::Insert => format!(
            "INSERT INTO {table} ({})\nVALUES (\n{}\n);\n",
            names.join(", "),
            lines(
                columns
                    .iter()
                    .map(|c| (
                        "    NULL".to_string(),
                        format!("{} {}", c.name, c.data_type)
                    ))
                    .collect()
            )
            .join("\n")
        ),
        Template::Update => {
            let set = lines(
                columns
                    .iter()
                    .zip(&names)
                    .filter(|(c, _)| !c.primary_key)
                    .map(|(c, n)| (format!("{n} = NULL"), c.data_type.clone()))
                    .collect(),
            );
            let keys: Vec<String> = columns
                .iter()
                .zip(&names)
                .filter(|(c, _)| c.primary_key)
                .map(|(_, n)| format!("{n} = NULL"))
                .collect();
            format!(
                "UPDATE {table}\nSET {}\nWHERE {};\n",
                set.join("\n    "),
                if keys.is_empty() {
                    // no key to aim at: match nothing until a condition is written
                    "1 = 0".to_string()
                } else {
                    keys.join(" AND ")
                }
            )
        }
    }
}

impl Focusable for ObjectBrowser {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for ObjectBrowser {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let filter_focused = self.filter.focus_handle(cx).is_focused(window);
        let count = self.rows.len();
        let list = uniform_list(
            "browser-rows",
            count,
            cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                range
                    .filter_map(|ix| this.rows.get(ix).cloned().map(|r| (ix, r)))
                    .map(|(ix, row)| {
                        let selected = this.selected.as_ref() == Some(&row.path);
                        let arrow = if !row.item.expandable() {
                            " "
                        } else if row.expanded {
                            "▾"
                        } else {
                            "▸"
                        };
                        let lead = match &row.item {
                            Item::Connection(c) => color_dot(c.color).into_any_element(),
                            item => div()
                                .text_color(Theme::muted())
                                .child(item.icon())
                                .into_any_element(),
                        };
                        div()
                            .id(("row", ix))
                            .w_full()
                            .h(ROW_HEIGHT)
                            .flex()
                            .items_center()
                            .gap_1p5()
                            .pl(px(6. + INDENT * row.depth as f32))
                            .pr_2()
                            .whitespace_nowrap()
                            .cursor_pointer()
                            .when(selected, |d| d.bg(Theme::accent_dim()))
                            .hover(|d| d.bg(Theme::border()))
                            .child(
                                div()
                                    .w(px(10.))
                                    .flex_none()
                                    .text_color(Theme::muted())
                                    .child(arrow),
                            )
                            .child(div().flex_none().child(lead))
                            .child(row.item.label())
                            .children(row.item.detail().map(|d| {
                                div().text_color(Theme::muted()).text_size(px(11.)).child(d)
                            }))
                            .children(row.note.clone().map(|(n, err)| {
                                div()
                                    .text_color(if err { Theme::error() } else { Theme::muted() })
                                    .text_size(px(11.))
                                    .overflow_hidden()
                                    .child(n)
                            }))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, e, window, cx| {
                                    this.select(ix, e, window, cx)
                                }),
                            )
                            .on_mouse_down(
                                MouseButton::Right,
                                cx.listener(move |this, e, window, cx| {
                                    this.select(ix, e, window, cx)
                                }),
                            )
                    })
                    .collect()
            }),
        )
        .track_scroll(self.scroll.clone())
        .flex_1();

        let menu = self.menu.as_ref().map(|m| {
            let actions = menu_for(&m.item);
            deferred(
                anchored().position(m.position).child(
                    div()
                        .occlude()
                        .min_w(px(180.))
                        .py_1()
                        .bg(Theme::panel())
                        .border_1()
                        .border_color(Theme::border())
                        .rounded_md()
                        .shadow_lg()
                        .text_size(px(12.))
                        .children(actions.into_iter().enumerate().map(|(i, (label, action))| {
                            div()
                                .id(("menu", i))
                                .px_3()
                                .py_0p5()
                                .cursor_pointer()
                                .hover(|d| d.bg(Theme::accent_dim()))
                                .child(label)
                                .on_click(
                                    cx.listener(move |this, _, _, cx| this.menu_action(action, cx)),
                                )
                        })),
                ),
            )
            .with_priority(1)
        });

        div()
            .key_context("ObjectBrowser")
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .bg(Theme::panel())
            .text_color(Theme::text())
            .font_family(theme::font_family(cx))
            .text_size(px(12.))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                if this.menu.take().is_some() {
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &editor_view::Cancel, window, cx| {
                if this.menu.take().is_some() {
                    cx.notify();
                } else {
                    this.filter.update(cx, |f, cx| f.set_text("", cx));
                    window.focus(&this.focus);
                }
            }))
            .child(
                div().p_1p5().flex_none().child(
                    div()
                        .h(px(24.))
                        .px_1p5()
                        .flex()
                        .items_center()
                        .bg(Theme::input_bg())
                        .rounded_sm()
                        .border_1()
                        .border_color(if filter_focused {
                            Theme::accent()
                        } else {
                            Theme::border()
                        })
                        .child(
                            div()
                                .flex_1()
                                .h(px(18.))
                                .overflow_hidden()
                                .child(self.filter.clone()),
                        ),
                ),
            )
            .when(count == 0, |d| {
                d.child(
                    div()
                        .p_3()
                        .text_color(Theme::muted())
                        .child(if self.search.is_some() {
                            "Nothing matches."
                        } else {
                            "No saved connections. Add one with Cmd/Ctrl+Shift+C."
                        }),
                )
            })
            .child(list)
            .children(menu)
    }
}

impl ObjectBrowser {
    /// Picks up connections added, renamed or deleted in the dialog.
    pub fn connections_changed(&mut self, cx: &mut Context<Self>) {
        let ids: HashSet<String> = cx
            .global::<Connections>()
            .store
            .connections()
            .iter()
            .map(|c| c.id.clone())
            .collect();
        let gone: Vec<String> = self
            .conns
            .keys()
            .filter(|id| !ids.contains(*id))
            .cloned()
            .collect();
        for id in gone {
            if let Some(Conn::Open(s)) = self.conns.remove(&id) {
                cx.background_spawn(async move { s.close().await }).detach();
            }
        }
        self.rebuild(cx);
    }

    /// Breaks a connection's session the way a dropped connection would.
    #[cfg(test)]
    pub fn drop_session(&self, cx: &App) -> Task<()> {
        let sessions: Vec<Session> = self
            .conns
            .values()
            .filter_map(|c| match c {
                Conn::Open(s) => Some(s.clone()),
                _ => None,
            })
            .collect();
        cx.background_spawn(async move {
            for s in sessions {
                let mut h = s.execute("lose");
                while h.next().await.is_some() {}
            }
        })
    }

    /// Notes shown on rows (loading, errors), for tests.
    #[cfg(test)]
    pub fn notes(&self) -> Vec<String> {
        self.rows
            .iter()
            .filter_map(|r| r.note.as_ref().map(|(n, _)| n.clone()))
            .collect()
    }

    #[cfg(test)]
    pub fn is_idle(&self) -> bool {
        !self
            .children
            .values()
            .any(|c| matches!(c, Children::Loading))
            && !self.conns.values().any(|c| matches!(c, Conn::Opening))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, ty: &str, pk: bool) -> ColumnInfo {
        ColumnInfo {
            name: name.into(),
            data_type: ty.into(),
            nullable: !pk,
            default: None,
            primary_key: pk,
            comment: None,
        }
    }

    #[test]
    fn templates() {
        let t = ObjectRef::new(ObjectKind::Table, "users").in_database("app");
        let cols = [col("id", "int", true), col("email", "varchar(255)", false)];
        assert_eq!(
            template(DriverKind::MySql, &t, &cols, Template::Select),
            "SELECT `id`, `email`\nFROM `app`.`users`\nLIMIT 100;\n"
        );
        assert_eq!(
            template(DriverKind::MySql, &t, &cols, Template::Insert),
            "INSERT INTO `app`.`users` (`id`, `email`)\nVALUES (\n    NULL, -- id int\n    NULL -- email varchar(255)\n);\n"
        );
        let update = template(DriverKind::MySql, &t, &cols, Template::Update);
        assert_eq!(
            update,
            "UPDATE `app`.`users`\nSET `email` = NULL -- varchar(255)\nWHERE `id` = NULL;\n"
        );
    }
}
