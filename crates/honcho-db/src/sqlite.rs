//! SQLite, over rusqlite with its own bundled SQLite.
//!
//! A connection is a file; `host` holds its path. rusqlite blocks, so the
//! connection moves to a blocking thread for each call, and rows come
//! back through a channel while a query runs. Attached databases show up
//! as databases, but the connection always stays on `main`.

mod split;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use rusqlite::types::ValueRef;
use rusqlite::{ErrorCode, InterruptHandle, OpenFlags};
use tokio::sync::mpsc;

use crate::script::Statement;
use crate::types::*;
use crate::{Canceller, Connection, DbError, Driver, Result, ResultSink, Value};

pub use split::split;

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

pub struct SqliteDriver;

#[async_trait]
impl Driver for SqliteDriver {
    fn kind(&self) -> DriverKind {
        DriverKind::Sqlite
    }

    fn default_port(&self) -> Option<u16> {
        None
    }

    async fn connect(&self, config: &ConnectConfig) -> Result<Box<dyn Connection>> {
        let path = file_path(&config.host)?;
        let read_only = config.read_only;
        let conn = tokio::task::spawn_blocking(move || open(&path, read_only))
            .await
            .map_err(|e| DbError::Connect(e.to_string()))??;
        Ok(Box::new(SqliteConnection {
            interrupt: Arc::new(conn.get_interrupt_handle()),
            conn: Some(conn),
            cancelled: Mutex::default(),
        }))
    }
}

fn file_path(host: &str) -> Result<PathBuf> {
    let host = host.trim();
    if host.is_empty() {
        return Err(DbError::Connect("no database file given".into()));
    }
    Ok(match host.strip_prefix("~/") {
        Some(rest) => dirs_home().map_or_else(|| PathBuf::from(host), |h| h.join(rest)),
        None => PathBuf::from(host),
    })
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from)
}

fn open(path: &std::path::Path, read_only: bool) -> Result<rusqlite::Connection> {
    // never CREATE: a mistyped path shouldn't leave an empty database behind
    let flags = if read_only {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    } else {
        OpenFlags::SQLITE_OPEN_READ_WRITE
    } | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = rusqlite::Connection::open_with_flags(path, flags)
        .map_err(|e| DbError::Connect(format!("{}: {}", path.display(), message(&e))))?;
    conn.busy_timeout(BUSY_TIMEOUT)
        .map_err(|e| DbError::Connect(e.to_string()))?;
    // opening is lazy; reading the schema shows it's really a database
    conn.query_row("SELECT count(*) FROM sqlite_schema", [], |_| Ok(()))
        .map_err(|e| DbError::Connect(format!("{}: {}", path.display(), message(&e))))?;
    Ok(conn)
}

pub struct SqliteConnection {
    conn: Option<rusqlite::Connection>,
    interrupt: Arc<InterruptHandle>,
    /// Set by the canceller handed out for the current run.
    cancelled: Mutex<Arc<AtomicBool>>,
}

impl SqliteConnection {
    /// Runs `f` with the connection on a blocking thread.
    async fn with<T, F>(&mut self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut rusqlite::Connection) -> Result<T> + Send + 'static,
    {
        let mut conn = self.conn.take().ok_or_else(closed)?;
        let (conn, out) = tokio::task::spawn_blocking(move || {
            let out = f(&mut conn);
            (conn, out)
        })
        .await
        .map_err(|e| DbError::ConnectionLost(e.to_string()))?;
        self.conn = Some(conn);
        out
    }

    async fn database_names(&mut self) -> Result<Vec<String>> {
        self.with(|c| {
            let mut stmt =
                c.prepare("SELECT name FROM pragma_database_list WHERE name <> 'temp'")?;
            let names = stmt.query_map([], |r| r.get(0))?;
            Ok(names.collect::<rusqlite::Result<_>>()?)
        })
        .await
    }

    async fn find(
        &mut self,
        databases: Vec<String>,
        filter: &'static str,
        term: String,
        limit: usize,
    ) -> Result<Vec<ObjectRef>> {
        self.with(move |c| {
            let mut out = Vec::new();
            for db in databases {
                let left = limit.saturating_sub(out.len());
                if left == 0 {
                    break;
                }
                let sql = format!(
                    "SELECT type, name FROM {}.sqlite_schema WHERE {USER_OBJECTS} AND {filter} \
                     ORDER BY name LIMIT ?2",
                    ident(&db)
                );
                let mut stmt = c.prepare(&sql)?;
                let rows = stmt.query_map(rusqlite::params![term, left as i64], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })?;
                for row in rows {
                    let (ty, name) = row?;
                    if let Some(o) = object(&ty, name, &db) {
                        out.push(o);
                    }
                }
            }
            Ok(out)
        })
        .await
    }
}

const USER_OBJECTS: &str =
    r"type IN ('table', 'view', 'trigger') AND name NOT LIKE 'sqlite\_%' ESCAPE '\'";

fn ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn object(ty: &str, name: String, database: &str) -> Option<ObjectRef> {
    let kind = match ty {
        "table" => ObjectKind::Table,
        "view" => ObjectKind::View,
        "trigger" => ObjectKind::Trigger,
        _ => return None,
    };
    Some(ObjectRef::new(kind, name).in_database(database))
}

fn database_of(o: &ObjectRef) -> String {
    o.database.clone().unwrap_or_else(|| "main".into())
}

fn closed() -> DbError {
    DbError::ConnectionLost("connection closed".into())
}

#[async_trait]
impl Connection for SqliteConnection {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            schemas: false,
            // attached databases are reachable as db.table
            cross_database: true,
        }
    }

    async fn execute(&mut self, sql: &str, sink: &mut ResultSink) -> Result<()> {
        let statements: Vec<(String, u32)> = split(sql)
            .into_iter()
            .map(|s| (s.text.to_string(), s.line))
            .collect();
        let cancelled = self.cancelled.lock().unwrap().clone();
        let (tx, mut rx) = mpsc::channel(256);
        let mut conn = self.conn.take().ok_or_else(closed)?;
        let flag = cancelled.clone();
        let job = tokio::task::spawn_blocking(move || {
            let out = run(&mut conn, &statements, &tx, &flag);
            (conn, out)
        });
        let mut sent = Ok(());
        while let Some(out) = rx.recv().await {
            if sent.is_err() {
                continue;
            }
            sent = match out {
                Out::Columns(cols) => sink.columns(cols).await,
                Out::Row(row) => sink.row(row).await,
                Out::Done(n) => sink.done(n).await,
            };
            if sent.is_err() {
                // nobody is reading any more: stop the query too
                cancelled.store(true, Ordering::SeqCst);
                self.interrupt.interrupt();
            }
        }
        let (conn, out) = job
            .await
            .map_err(|e| DbError::ConnectionLost(e.to_string()))?;
        self.conn = Some(conn);
        sent?;
        out
    }

    fn statements<'a>(&self, sql: &'a str) -> Vec<Statement<'a>> {
        split(sql)
    }

    fn canceller(&self) -> Option<Box<dyn Canceller>> {
        let flag = Arc::new(AtomicBool::new(false));
        *self.cancelled.lock().unwrap() = flag.clone();
        Some(Box::new(SqliteCanceller {
            flag,
            interrupt: self.interrupt.clone(),
        }))
    }

    async fn ping(&mut self) -> Result<()> {
        self.with(|c| Ok(c.query_row("SELECT 1", [], |_| Ok(()))?))
            .await
    }

    async fn current_database(&mut self) -> Result<Option<String>> {
        Ok(Some("main".into()))
    }

    async fn use_database(&mut self, name: &str) -> Result<()> {
        if name == "main" {
            return Ok(());
        }
        if self.database_names().await?.iter().any(|d| d == name) {
            return Err(DbError::Unsupported("switching away from main"));
        }
        Err(DbError::query(format!("no database {name}")))
    }

    async fn in_transaction(&mut self) -> Result<bool> {
        Ok(!self.conn.as_ref().ok_or_else(closed)?.is_autocommit())
    }

    async fn databases(&mut self) -> Result<Vec<String>> {
        self.database_names().await
    }

    async fn objects(&mut self, scope: &Scope, kinds: &[ObjectKind]) -> Result<Vec<ObjectRef>> {
        let db = scope.database.clone().unwrap_or_else(|| "main".into());
        let kinds = kinds.to_vec();
        self.with(move |c| {
            let sql = format!(
                "SELECT type, name FROM {}.sqlite_schema WHERE {USER_OBJECTS} ORDER BY name",
                ident(&db)
            );
            let mut stmt = c.prepare(&sql)?;
            let rows =
                stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            let mut out = Vec::new();
            for row in rows {
                let (ty, name) = row?;
                if let Some(o) = object(&ty, name, &db) {
                    if kinds.is_empty() || kinds.contains(&o.kind) {
                        out.push(o);
                    }
                }
            }
            Ok(out)
        })
        .await
    }

    async fn columns(&mut self, o: &ObjectRef) -> Result<Vec<ColumnInfo>> {
        let (db, name) = (database_of(o), o.name.clone());
        self.with(move |c| {
            let mut stmt = c.prepare(
                "SELECT name, type, \"notnull\", dflt_value, pk FROM pragma_table_info(?1, ?2) \
                 ORDER BY cid",
            )?;
            let cols = stmt
                .query_map([&name, &db], |r| {
                    let pk = r.get::<_, i64>(4)? > 0;
                    Ok(ColumnInfo {
                        name: r.get(0)?,
                        data_type: r.get(1)?,
                        nullable: r.get::<_, i64>(2)? == 0 && !pk,
                        default: r.get(3)?,
                        primary_key: pk,
                        comment: None,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            if cols.is_empty() {
                return Err(DbError::query(format!("no table or view {name}")));
            }
            Ok(cols)
        })
        .await
    }

    async fn indexes(&mut self, table: &ObjectRef) -> Result<Vec<IndexInfo>> {
        let (db, name) = (database_of(table), table.name.clone());
        self.with(move |c| {
            let mut list =
                c.prepare("SELECT name, \"unique\", origin = 'pk' FROM pragma_index_list(?1, ?2)")?;
            let found = list
                .query_map([&name, &db], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, bool>(1)?,
                        r.get::<_, bool>(2)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut info = c.prepare(
                "SELECT coalesce(name, '(expression)') FROM pragma_index_info(?1, ?2) \
                 ORDER BY seqno",
            )?;
            let mut out = Vec::new();
            for (index, unique, primary) in found {
                let columns = info
                    .query_map([&index, &db], |r| r.get(0))?
                    .collect::<rusqlite::Result<Vec<String>>>()?;
                out.push(IndexInfo {
                    name: index,
                    columns,
                    unique,
                    primary,
                });
            }
            // an INTEGER PRIMARY KEY is the rowid itself, with no index of its own
            if !out.iter().any(|i| i.primary) {
                let mut pk = c.prepare(
                    "SELECT name FROM pragma_table_info(?1, ?2) WHERE pk > 0 ORDER BY pk",
                )?;
                let columns = pk
                    .query_map([&name, &db], |r| r.get(0))?
                    .collect::<rusqlite::Result<Vec<String>>>()?;
                if !columns.is_empty() {
                    out.push(IndexInfo {
                        name: "PRIMARY KEY".into(),
                        columns,
                        unique: true,
                        primary: true,
                    });
                }
            }
            out.sort_by(|a, b| b.primary.cmp(&a.primary).then(a.name.cmp(&b.name)));
            Ok(out)
        })
        .await
    }

    async fn search(&mut self, term: &str, limit: usize) -> Result<Vec<ObjectRef>> {
        let dbs = self.database_names().await?;
        self.find(dbs, "instr(lower(name), lower(?1)) > 0", term.into(), limit)
            .await
    }

    async fn search_bodies(&mut self, term: &str, limit: usize) -> Result<Vec<ObjectRef>> {
        let dbs = self.database_names().await?;
        self.find(
            dbs,
            "type <> 'table' AND instr(lower(sql), lower(?1)) > 0",
            term.into(),
            limit,
        )
        .await
    }

    async fn parameters(&mut self, _routine: &ObjectRef) -> Result<Vec<RoutineParam>> {
        Err(DbError::Unsupported("routines in SQLite"))
    }

    async fn definition(&mut self, o: &ObjectRef) -> Result<String> {
        let ty = match o.kind {
            ObjectKind::Table => "table",
            ObjectKind::View => "view",
            ObjectKind::Trigger => "trigger",
            _ => return Err(DbError::Unsupported("this kind of object in SQLite")),
        };
        let (db, name) = (database_of(o), o.name.clone());
        self.with(move |c| {
            let schema = format!("{}.sqlite_schema", ident(&db));
            let sql: Option<String> = c
                .query_row(
                    &format!("SELECT sql FROM {schema} WHERE type = ?1 AND name = ?2"),
                    [ty, &name],
                    |r| r.get(0),
                )
                .map_err(|e| match e {
                    rusqlite::Error::QueryReturnedNoRows => {
                        DbError::query(format!("no {ty} {name}"))
                    }
                    e => e.into(),
                })?;
            let mut out = sql.unwrap_or_default();
            if ty == "table" {
                // the table's own indexes; those behind constraints have no sql
                let mut stmt = c.prepare(&format!(
                    "SELECT sql FROM {schema} WHERE type = 'index' AND tbl_name = ?1 \
                     AND sql IS NOT NULL ORDER BY name"
                ))?;
                for sql in stmt.query_map([&name], |r| r.get::<_, String>(0))? {
                    out = format!("{out};\n{}", sql?);
                }
            }
            Ok(out)
        })
        .await
    }

    async fn close(&mut self) -> Result<()> {
        if let Some(conn) = self.conn.take() {
            let _ = tokio::task::spawn_blocking(move || conn.close()).await;
        }
        Ok(())
    }
}

enum Out {
    Columns(Vec<Column>),
    Row(Vec<Value>),
    Done(Option<u64>),
}

/// Runs the statements in order, stopping at the first error.
fn run(
    conn: &mut rusqlite::Connection,
    statements: &[(String, u32)],
    tx: &mpsc::Sender<Out>,
    cancelled: &AtomicBool,
) -> Result<()> {
    let send = |out| tx.blocking_send(out).map_err(|_| DbError::Cancelled);
    for (text, line) in statements {
        if cancelled.load(Ordering::SeqCst) {
            return Err(DbError::Cancelled);
        }
        let fail = |e| statement_error(e, text, *line, cancelled);
        let mut stmt = conn.prepare(text).map_err(fail)?;
        if stmt.column_count() == 0 {
            let changed = stmt.raw_execute().map_err(fail)? as u64;
            send(Out::Done(counts_rows(text).then_some(changed)))?;
            continue;
        }
        let (columns, types): (Vec<Column>, Vec<String>) = stmt
            .columns()
            .map(|c| {
                let ty = c.decl_type().unwrap_or("").to_string();
                (Column::new(c.name(), ty.as_str()), ty)
            })
            .unzip();
        send(Out::Columns(columns))?;
        let mut rows = stmt.raw_query();
        while let Some(row) = rows.next().map_err(fail)? {
            let values = types
                .iter()
                .enumerate()
                .map(|(i, ty)| value(row.get_ref_unwrap(i), ty))
                .collect();
            send(Out::Row(values))?;
        }
        send(Out::Done(None))?;
    }
    Ok(())
}

/// Whether the statement's count is rows it changed; SQLite keeps the
/// last DML count around through other statements.
fn counts_rows(sql: &str) -> bool {
    let code = &sql[crate::routine::code_start(sql)..];
    let first = code
        .split(|c: char| !c.is_ascii_alphabetic())
        .next()
        .unwrap_or("");
    ["INSERT", "UPDATE", "DELETE", "REPLACE", "WITH"]
        .iter()
        .any(|w| w.eq_ignore_ascii_case(first))
}

/// A value, read by its column's declared type where SQLite's own storage
/// class says less (dates are text, booleans integers).
fn value(v: ValueRef<'_>, declared: &str) -> Value {
    let ty = declared.to_ascii_uppercase();
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) if ty.starts_with("BOOL") => Value::Bool(i != 0),
        ValueRef::Integer(i) => Value::Int(i),
        ValueRef::Real(f) => Value::Float(f),
        ValueRef::Text(t) => {
            let s = String::from_utf8_lossy(t).into_owned();
            if ty.starts_with("DATETIME") || ty.starts_with("TIMESTAMP") {
                Value::DateTime(s)
            } else if ty.starts_with("DATE") {
                Value::Date(s)
            } else if ty.starts_with("TIME") {
                Value::Time(s)
            } else if ty.starts_with("JSON") {
                Value::Json(s)
            } else {
                Value::Text(s)
            }
        }
        ValueRef::Blob(b) => Value::Bytes(b.to_vec()),
    }
}

struct SqliteCanceller {
    flag: Arc<AtomicBool>,
    interrupt: Arc<InterruptHandle>,
}

#[async_trait]
impl Canceller for SqliteCanceller {
    async fn cancel(&self) -> Result<()> {
        self.flag.store(true, Ordering::SeqCst);
        self.interrupt.interrupt();
        Ok(())
    }
}

/// SQLite's own message, without rusqlite's wrapping.
fn message(e: &rusqlite::Error) -> String {
    match e {
        rusqlite::Error::SqliteFailure(_, Some(m)) => m.clone(),
        rusqlite::Error::SqlInputError { msg, .. } => msg.clone(),
        e => e.to_string(),
    }
}

fn interrupted(e: &rusqlite::Error) -> bool {
    e.sqlite_error_code() == Some(ErrorCode::OperationInterrupted)
}

/// An error in a statement of the script, at the line it's on; the
/// position SQLite gives, when it gives one, narrows that down.
fn statement_error(
    e: rusqlite::Error,
    statement: &str,
    line: u32,
    cancelled: &AtomicBool,
) -> DbError {
    if interrupted(&e) || cancelled.load(Ordering::SeqCst) {
        return DbError::Cancelled;
    }
    let line = match &e {
        rusqlite::Error::SqlInputError { offset, .. } if *offset >= 0 => {
            let at = (*offset as usize).min(statement.len());
            line + statement.as_bytes()[..at]
                .iter()
                .filter(|&&b| b == b'\n')
                .count() as u32
        }
        _ => line,
    };
    DbError::Query {
        message: message(&e),
        code: e.sqlite_error().map(|f| f.extended_code.to_string()),
        line: Some(line),
    }
}

impl From<rusqlite::Error> for DbError {
    fn from(e: rusqlite::Error) -> Self {
        if interrupted(&e) {
            return DbError::Cancelled;
        }
        DbError::Query {
            message: message(&e),
            code: e.sqlite_error().map(|f| f.extended_code.to_string()),
            line: None,
        }
    }
}

#[cfg(test)]
mod tests;
