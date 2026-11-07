//! What completion knows about each connection's schema, fetched as it's
//! needed. Each connection's catalog has its own session so completion
//! never waits behind a query running in a tab.

use std::collections::{HashMap, HashSet};

use gpui::{App, AppContext, Context, Entity, Global};
use honcho_config::SavedConnection;
use honcho_core::complete::{Dialect, Kind, Need, Schema, SchemaColumn, SchemaObject};
use honcho_db::{DriverKind, ObjectKind, ObjectRef, Scope, Session};

use crate::connections;

#[derive(Default)]
pub struct Catalogs(HashMap<String, Entity<Catalog>>);

impl Global for Catalogs {}

impl Catalogs {
    /// The catalog for a saved connection, made on first use.
    pub fn get(saved: &SavedConnection, cx: &mut App) -> Entity<Catalog> {
        if !cx.has_global::<Catalogs>() {
            cx.set_global(Catalogs::default());
        }
        if let Some(c) = cx.global::<Catalogs>().0.get(&saved.id) {
            return c.clone();
        }
        let catalog = cx.new(|_| Catalog::new(saved.clone()));
        cx.global_mut::<Catalogs>()
            .0
            .insert(saved.id.clone(), catalog.clone());
        catalog
    }
}

enum Conn {
    Closed,
    Opening,
    Open(Session),
    /// Opening failed; completion carries on with keywords only.
    Failed,
}

pub struct Catalog {
    saved: SavedConnection,
    conn: Conn,
    databases: Option<Vec<String>>,
    objects: HashMap<String, Vec<SchemaObject>>,
    /// By database, schema (when the table was named with one) and table.
    columns: HashMap<(String, Option<String>, String), Vec<SchemaColumn>>,
    /// Asked for and not back yet, or failed: not asked again until a clear.
    asked: HashSet<Need>,
    /// Needs waiting for the session to open.
    queued: Vec<Need>,
    /// Reopened after the session dropped; dropping again is reported.
    reconnected: bool,
    /// Why completion is down to keywords, when the connection is gone.
    lost: Option<String>,
}

impl Catalog {
    fn new(saved: SavedConnection) -> Self {
        Self {
            saved,
            conn: Conn::Closed,
            databases: None,
            objects: HashMap::new(),
            columns: HashMap::new(),
            asked: HashSet::new(),
            queued: Vec::new(),
            reconnected: false,
            lost: None,
        }
    }

    /// Forgets everything, e.g. after DDL ran or on request.
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.databases = None;
        self.objects.clear();
        self.columns.clear();
        self.asked.clear();
        // and a lost connection gets another go
        if matches!(self.conn, Conn::Failed) {
            self.conn = Conn::Closed;
        }
        self.reconnected = false;
        self.lost = None;
        cx.notify();
    }

    /// Breaks the session the way a dropped connection would.
    #[cfg(test)]
    pub fn drop_session(&self, cx: &App) -> gpui::Task<()> {
        let session = match &self.conn {
            Conn::Open(s) => Some(s.clone()),
            _ => None,
        };
        cx.background_spawn(async move {
            if let Some(s) = session {
                let mut h = s.execute("lose");
                while h.next().await.is_some() {}
            }
        })
    }

    pub fn lost(&self) -> Option<&str> {
        self.lost.as_deref()
    }

    /// The catalog seen from a tab on `current` database.
    pub fn view<'a>(&'a self, current: Option<&'a str>) -> View<'a> {
        View {
            catalog: Some(self),
            current,
        }
    }

    /// Starts loading whatever completion was missing. Notifies as each
    /// piece arrives.
    pub fn fetch(&mut self, needs: &[Need], cx: &mut Context<Self>) {
        let new: Vec<Need> = needs
            .iter()
            .filter(|n| self.asked.insert((*n).clone()))
            .cloned()
            .collect();
        if new.is_empty() {
            return;
        }
        let session = match &self.conn {
            Conn::Open(s) => s.clone(),
            Conn::Failed => return,
            Conn::Opening => {
                self.queued.extend(new);
                return;
            }
            Conn::Closed => {
                self.queued.extend(new);
                self.open(cx);
                return;
            }
        };
        for need in new {
            self.load(session.clone(), need, cx);
        }
    }

    fn open(&mut self, cx: &mut Context<Self>) {
        self.conn = Conn::Opening;
        let opening = connections::open_session(self.saved.clone(), None, cx);
        cx.spawn(async move |this, cx| {
            let result = opening.await;
            this.update(cx, |this, cx| match result {
                Ok(session) => {
                    this.conn = Conn::Open(session.clone());
                    for need in std::mem::take(&mut this.queued) {
                        this.load(session.clone(), need, cx);
                    }
                }
                Err(e) => {
                    this.conn = Conn::Failed;
                    this.queued.clear();
                    if this.reconnected {
                        this.lost = Some(format!("Connection lost and reconnecting failed: {e}"));
                        cx.notify();
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    fn load(&mut self, session: Session, need: Need, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let loaded = match &need {
                Need::Databases => session.databases().await.map(Loaded::Databases),
                Need::Objects(db) => session
                    .objects(
                        Scope {
                            database: Some(db.clone()),
                            schema: None,
                        },
                        vec![],
                    )
                    .await
                    .map(|o| Loaded::Objects(o.iter().filter_map(schema_object).collect())),
                Need::Columns(db, schema, table) => session
                    .columns(ObjectRef {
                        schema: schema.clone(),
                        ..ObjectRef::new(ObjectKind::Table, table.clone()).in_database(db.clone())
                    })
                    .await
                    .map(|cols| {
                        Loaded::Columns(
                            cols.into_iter()
                                .map(|c| SchemaColumn {
                                    name: c.name,
                                    data_type: c.data_type,
                                })
                                .collect(),
                        )
                    }),
            };
            this.update(cx, |this, cx| {
                if let Err(e) = &loaded {
                    if e.is_fatal() {
                        this.dropped(need, e.to_string(), cx);
                        return;
                    }
                } else {
                    this.reconnected = false;
                }
                match (need, loaded) {
                    (Need::Databases, Ok(Loaded::Databases(d))) => this.databases = Some(d),
                    (Need::Objects(db), Ok(Loaded::Objects(o))) => {
                        this.objects.insert(db, o);
                    }
                    (Need::Columns(db, schema, t), Ok(Loaded::Columns(c))) => {
                        this.columns.insert((db, schema, t), c);
                    }
                    // a table that doesn't exist yet, say: leave it asked
                    _ => {}
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}

impl Catalog {
    /// The session dropped while loading `need`: reopen it once and ask
    /// again; if it drops again right away, say so.
    fn dropped(&mut self, need: Need, reason: String, cx: &mut Context<Self>) {
        // other loads cut off by the same drop wait for the same reconnect
        if matches!(self.conn, Conn::Opening) {
            self.queued.push(need);
            return;
        }
        if std::mem::replace(&mut self.reconnected, true) {
            self.conn = Conn::Failed;
            self.lost = Some(format!("Connection lost: {reason}"));
            cx.notify();
            return;
        }
        self.queued.push(need);
        if !matches!(self.conn, Conn::Opening) {
            self.open(cx);
        }
    }
}

enum Loaded {
    Databases(Vec<String>),
    Objects(Vec<SchemaObject>),
    Columns(Vec<SchemaColumn>),
}

fn schema_object(o: &ObjectRef) -> Option<SchemaObject> {
    let kind = match o.kind {
        ObjectKind::Table => Kind::Table,
        ObjectKind::View | ObjectKind::MaterializedView => Kind::View,
        ObjectKind::Procedure => Kind::Procedure,
        ObjectKind::Function => Kind::Function,
        ObjectKind::Trigger | ObjectKind::Sequence => return None,
    };
    Some(SchemaObject {
        schema: o.schema.clone(),
        name: o.name.clone(),
        kind,
        detail: o.signature.clone(),
    })
}

/// A catalog, or nothing (a tab without a connection still gets keywords),
/// with the tab's current database.
pub struct View<'a> {
    catalog: Option<&'a Catalog>,
    current: Option<&'a str>,
}

impl View<'_> {
    pub fn empty() -> View<'static> {
        View {
            catalog: None,
            current: None,
        }
    }
}

impl Schema for View<'_> {
    fn dialect(&self) -> Dialect {
        match self.catalog.map(|c| c.saved.driver) {
            Some(DriverKind::SqlServer) => Dialect::SqlServer,
            Some(DriverKind::Postgres) => Dialect::Postgres,
            Some(DriverKind::Sqlite) => Dialect::Sqlite,
            _ => Dialect::MySql,
        }
    }

    fn current_database(&self) -> Option<&str> {
        self.current
    }

    fn databases(&self) -> Option<&[String]> {
        self.catalog?.databases.as_deref()
    }

    fn objects(&self, database: &str) -> Option<&[SchemaObject]> {
        self.catalog?.objects.get(database).map(Vec::as_slice)
    }

    fn columns(
        &self,
        database: &str,
        schema: Option<&str>,
        table: &str,
    ) -> Option<&[SchemaColumn]> {
        let catalog = self.catalog?;
        let same = |a: &str, b: &str| a.eq_ignore_ascii_case(b);
        // names in SQL are often typed in another case than the server's
        catalog
            .columns
            .get(&(
                database.to_string(),
                schema.map(str::to_string),
                table.to_string(),
            ))
            .or_else(|| {
                catalog.columns.iter().find_map(|((d, s, t), c)| {
                    let schema_matches = match (s, schema) {
                        (Some(a), Some(b)) => same(a, b),
                        (None, None) => true,
                        _ => false,
                    };
                    (same(d, database) && schema_matches && same(t, table)).then_some(c)
                })
            })
            .map(Vec::as_slice)
    }
}
