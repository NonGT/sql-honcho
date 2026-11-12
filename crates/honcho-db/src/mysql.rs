//! MySQL and MariaDB, over `mysql_async`.

mod convert;
mod explain;
mod split;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use mysql_async::prelude::*;
use mysql_async::{ClientIdentity, Conn, DriverError, Opts, OptsBuilder, Row, SslOpts};

use crate::types::*;
use crate::{Canceller, Connection, DbError, Driver, Plan, Result, ResultSink};

pub use split::split;

const DEFAULT_PORT: u16 = 3306;
const ER_QUERY_INTERRUPTED: u16 = 1317;
const AUTH_ERRORS: &[u16] = &[
    1044, // access denied to database
    1045, // access denied
    1698, // no password for this user
    1862, // password expired
];

pub struct MySqlDriver;

#[async_trait]
impl Driver for MySqlDriver {
    fn kind(&self) -> DriverKind {
        DriverKind::MySql
    }

    fn default_port(&self) -> Option<u16> {
        Some(DEFAULT_PORT)
    }

    async fn connect(&self, config: &ConnectConfig) -> Result<Box<dyn Connection>> {
        crate::runtime::install_crypto_provider();
        let host = match config.host.as_str() {
            "" => "localhost",
            h => h,
        };
        let builder = OptsBuilder::default()
            .ip_or_hostname(host)
            .tcp_port(config.port.unwrap_or(DEFAULT_PORT))
            .user(Some(&config.user))
            .pass(config.password.clone())
            .db_name(config.database.clone())
            // by default it switches to the server's unix socket when the
            // host is local, which isn't there when the server is in Docker
            .prefer_socket(false);
        let tls = ssl_opts(&config.tls)?;
        let opts: Opts = builder.clone().ssl_opts(tls.clone()).into();
        let (mut conn, opts) = match Conn::new(opts.clone()).await {
            Ok(conn) => (conn, opts),
            // prefer settles for plain TCP when the server has no TLS
            Err(mysql_async::Error::Driver(DriverError::NoClientSslFlagFromServer))
                if config.tls.mode == TlsMode::Prefer =>
            {
                let opts: Opts = builder.ssl_opts(None::<SslOpts>).into();
                let conn = Conn::new(opts.clone()).await.map_err(connect_error)?;
                (conn, opts)
            }
            Err(e) => return Err(connect_error(e)),
        };
        if config.read_only {
            conn.query_drop("SET SESSION TRANSACTION READ ONLY")
                .await
                .map_err(connect_error)?;
        }
        let mut conn = MySqlConnection {
            conn: Some(conn),
            opts,
            cancelled: Mutex::new(Arc::default()),
            server: None,
        };
        // up front, so capabilities() knows whether Analyze works
        conn.server()
            .await
            .map_err(|e| DbError::Connect(e.to_string()))?;
        Ok(Box::new(conn))
    }
}

pub struct MySqlConnection {
    conn: Option<Conn>,
    opts: Opts,
    /// Set by the canceller handed out for the current run.
    cancelled: Mutex<Arc<AtomicBool>>,
    /// Asked for the first time it matters.
    server: Option<Server>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Server {
    MariaDb,
    /// `tree`: 8.3 or later, which explains as a tree of operators.
    MySql {
        tree: bool,
    },
}

impl Server {
    fn from_version(version: &str) -> Self {
        if version.contains("MariaDB") {
            return Server::MariaDb;
        }
        let mut parts = version
            .split(|c: char| !c.is_ascii_digit())
            .map(|p| p.parse::<u32>().unwrap_or(0));
        let major = parts.next().unwrap_or(0);
        let minor = parts.next().unwrap_or(0);
        Server::MySql {
            tree: (major, minor) >= (8, 3),
        }
    }
}

impl MySqlConnection {
    fn conn(&mut self) -> Result<&mut Conn> {
        self.conn
            .as_mut()
            .ok_or_else(|| DbError::ConnectionLost("connection closed".into()))
    }

    async fn server(&mut self) -> Result<Server> {
        if let Some(s) = self.server {
            return Ok(s);
        }
        let version: Option<String> = self
            .conn()?
            .query_first("SELECT VERSION()")
            .await
            .map_err(query_error)?;
        let s = Server::from_version(&version.unwrap_or_default());
        self.server = Some(s);
        Ok(s)
    }

    async fn database_or_current(&mut self, database: &Option<String>) -> Result<String> {
        match database {
            Some(db) => Ok(db.clone()),
            None => self
                .current_database()
                .await?
                .ok_or_else(|| DbError::query("no database selected")),
        }
    }
}

#[async_trait]
impl Connection for MySqlConnection {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            schemas: false,
            cross_database: true,
            explain: true,
            explain_analyze: self.server != Some(Server::MySql { tree: false }),
        }
    }

    async fn execute(&mut self, sql: &str, sink: &mut ResultSink) -> Result<()> {
        let cancelled = self.cancelled.lock().unwrap().clone();
        let conn = self.conn()?;
        for statement in split(sql) {
            let warnings = run(conn, statement.text, sink, &cancelled)
                .await
                .map_err(|e| at_line(e, statement.line))?;
            if cancelled.load(Ordering::SeqCst) {
                return Err(DbError::Cancelled);
            }
            if warnings > 0 {
                show_warnings(conn, sink).await?;
            }
        }
        Ok(())
    }

    async fn explain(&mut self, sql: &str, analyze: bool) -> Result<Plan> {
        let cancelled = self.cancelled.lock().unwrap().clone();
        let statements = split(sql);
        let statement = statements
            .iter()
            .find(|s| !s.text.trim().is_empty())
            .ok_or_else(|| DbError::Other("Nothing to explain.".into()))?;
        let text = statement.text.trim_end().trim_end_matches(';');
        let server = self.server().await?;
        if analyze && server == (Server::MySql { tree: false }) {
            return Err(DbError::Other(
                "Explain Analyze needs MySQL 8.3 or later.".into(),
            ));
        }
        // ANALYZE runs the statement; undo it, keeping the user's own
        // transaction if there is one
        let (open, close): (&[&str], &[&str]) = match analyze {
            false => (&[], &[]),
            true if self.in_transaction().await? => (
                &["SAVEPOINT honcho_explain"],
                &[
                    "ROLLBACK TO SAVEPOINT honcho_explain",
                    "RELEASE SAVEPOINT honcho_explain",
                ],
            ),
            true => (&["START TRANSACTION"], &["ROLLBACK"]),
        };
        let conn = self.conn()?;
        if analyze && crate::script::write_reason(text).is_some() {
            let stuck = non_transactional(conn, server, text).await?;
            if !stuck.is_empty() {
                return Err(DbError::Other(format!(
                    "Explain Analyze won't run this: {} can't roll back, so the \
                     changes would stay. Use Explain instead.",
                    stuck.join(", ")
                )));
            }
        }
        for sql in open {
            conn.query_drop(*sql).await.map_err(query_error)?;
        }
        let result = explain_on(conn, server, text, analyze)
            .await
            .map_err(|e| at_line(e, statement.line));
        for sql in close {
            let undone = conn.query_drop(*sql).await.map_err(query_error);
            if result.is_ok() {
                undone?;
            }
        }
        if cancelled.load(Ordering::SeqCst) {
            return Err(DbError::Cancelled);
        }
        result
    }

    fn statements<'a>(&self, sql: &'a str) -> Vec<crate::script::Statement<'a>> {
        split(sql)
    }

    fn canceller(&self) -> Option<Box<dyn Canceller>> {
        let conn = self.conn.as_ref()?;
        let flag = Arc::new(AtomicBool::new(false));
        *self.cancelled.lock().unwrap() = flag.clone();
        Some(Box::new(MySqlCanceller {
            flag,
            opts: OptsBuilder::from_opts(self.opts.clone())
                .db_name(None::<String>)
                .into(),
            id: conn.id(),
        }))
    }

    async fn ping(&mut self) -> Result<()> {
        self.conn()?.ping().await.map_err(query_error)
    }

    async fn in_transaction(&mut self) -> Result<bool> {
        // every OK packet says whether a transaction is open; DO 0 just
        // gets a fresh one, without the PROCESS privilege innodb_trx needs
        let conn = self.conn()?;
        conn.query_drop("DO 0").await.map_err(query_error)?;
        Ok(conn.last_ok_packet().is_some_and(|ok| {
            ok.status_flags()
                .contains(mysql_async::consts::StatusFlags::SERVER_STATUS_IN_TRANS)
        }))
    }

    async fn current_database(&mut self) -> Result<Option<String>> {
        let db: Option<Option<String>> = self
            .conn()?
            .query_first("SELECT DATABASE()")
            .await
            .map_err(query_error)?;
        Ok(db.flatten())
    }

    async fn use_database(&mut self, name: &str) -> Result<()> {
        self.conn()?
            .query_drop(format!("USE {}", quote(name)))
            .await
            .map_err(query_error)
    }

    async fn databases(&mut self) -> Result<Vec<String>> {
        self.conn()?
            .query("SHOW DATABASES")
            .await
            .map_err(query_error)
    }

    async fn objects(&mut self, scope: &Scope, kinds: &[ObjectKind]) -> Result<Vec<ObjectRef>> {
        let db = self.database_or_current(&scope.database).await?;
        let want = |k: ObjectKind| kinds.is_empty() || kinds.contains(&k);
        let conn = self.conn()?;
        let mut found: Vec<(ObjectKind, String, Option<String>)> = Vec::new();

        if want(ObjectKind::Table) || want(ObjectKind::View) || want(ObjectKind::Sequence) {
            let rows: Vec<(String, String)> = conn
                .exec(
                    "SELECT table_name, table_type FROM information_schema.tables \
                     WHERE table_schema = ? ORDER BY table_name",
                    (&db,),
                )
                .await
                .map_err(query_error)?;
            found.extend(rows.into_iter().map(|(name, ty)| {
                let kind = match ty.as_str() {
                    "VIEW" | "SYSTEM VIEW" => ObjectKind::View,
                    "SEQUENCE" => ObjectKind::Sequence,
                    _ => ObjectKind::Table,
                };
                (kind, name, None)
            }));
        }
        if want(ObjectKind::Procedure) || want(ObjectKind::Function) {
            type RoutineRow = (String, String, Option<String>, Option<String>);
            let rows: Vec<RoutineRow> = conn
                .exec(
                    "SELECT r.routine_name, r.routine_type, r.dtd_identifier, \
                     (SELECT GROUP_CONCAT(CONCAT_WS(' ', p.parameter_mode, p.parameter_name, \
                       p.dtd_identifier) ORDER BY p.ordinal_position SEPARATOR ', ') \
                      FROM information_schema.parameters p \
                      WHERE p.specific_schema = r.routine_schema \
                        AND p.specific_name = r.specific_name AND p.ordinal_position > 0) \
                     FROM information_schema.routines r \
                     WHERE r.routine_schema = ? ORDER BY r.routine_name",
                    (&db,),
                )
                .await
                .map_err(query_error)?;
            // MariaDB also lists packages here
            found.extend(rows.into_iter().filter_map(|(name, ty, returns, params)| {
                let params = format!("({})", params.unwrap_or_default());
                match ty.as_str() {
                    "PROCEDURE" => Some((ObjectKind::Procedure, name, Some(params))),
                    "FUNCTION" => {
                        let returns = returns.map(|r| format!(" → {r}")).unwrap_or_default();
                        Some((ObjectKind::Function, name, Some(params + &returns)))
                    }
                    _ => None,
                }
            }));
        }
        if want(ObjectKind::Trigger) {
            let rows: Vec<String> = conn
                .exec(
                    "SELECT trigger_name FROM information_schema.triggers \
                     WHERE trigger_schema = ? ORDER BY trigger_name",
                    (&db,),
                )
                .await
                .map_err(query_error)?;
            found.extend(
                rows.into_iter()
                    .map(|name| (ObjectKind::Trigger, name, None)),
            );
        }

        Ok(found
            .into_iter()
            .filter(|(kind, ..)| want(*kind))
            .map(|(kind, name, signature)| ObjectRef {
                signature,
                ..ObjectRef::new(kind, name).in_database(db.clone())
            })
            .collect())
    }

    async fn columns(&mut self, object: &ObjectRef) -> Result<Vec<ColumnInfo>> {
        type ColumnRow = (String, String, String, Option<String>, String, String);
        let db = self.database_or_current(&object.database).await?;
        let rows: Vec<ColumnRow> = self
            .conn()?
            .exec(
                "SELECT column_name, column_type, is_nullable, column_default, column_key, \
                 column_comment FROM information_schema.columns \
                 WHERE table_schema = ? AND table_name = ? ORDER BY ordinal_position",
                (&db, &object.name),
            )
            .await
            .map_err(query_error)?;
        if rows.is_empty() {
            return Err(DbError::query(format!(
                "no table or view {}.{}",
                db, object.name
            )));
        }
        Ok(rows
            .into_iter()
            .map(
                |(name, data_type, nullable, default, key, comment)| ColumnInfo {
                    name,
                    data_type,
                    nullable: nullable == "YES",
                    default,
                    primary_key: key == "PRI",
                    comment: Some(comment).filter(|c| !c.is_empty()),
                },
            )
            .collect())
    }

    async fn indexes(&mut self, table: &ObjectRef) -> Result<Vec<IndexInfo>> {
        type IndexRow = (String, i64, String);
        let db = self.database_or_current(&table.database).await?;
        let rows: Vec<IndexRow> = self
            .conn()?
            .exec(
                // GROUP_CONCAT's separator can't appear in a column name
                "SELECT index_name, MIN(non_unique), \
                 GROUP_CONCAT(column_name ORDER BY seq_in_index SEPARATOR '\\0') \
                 FROM information_schema.statistics \
                 WHERE table_schema = ? AND table_name = ? \
                 GROUP BY index_name \
                 ORDER BY index_name <> 'PRIMARY', index_name",
                (&db, &table.name),
            )
            .await
            .map_err(query_error)?;
        Ok(rows
            .into_iter()
            .map(|(name, non_unique, columns)| IndexInfo {
                primary: name == "PRIMARY",
                unique: non_unique == 0,
                columns: columns.split('\0').map(str::to_string).collect(),
                name,
            })
            .collect())
    }

    async fn search(&mut self, term: &str, limit: usize) -> Result<Vec<ObjectRef>> {
        // MySQL 8's dictionary compares names case-sensitively, hence LOWER
        let pattern = format!("%{}%", like_escape(term));
        let rows: Vec<(String, String, String)> = self
            .conn()?
            .exec(
                "SELECT table_schema, table_name, table_type FROM information_schema.tables \
                 WHERE LOWER(table_name) LIKE LOWER(?) \
                 UNION ALL SELECT routine_schema, routine_name, routine_type \
                 FROM information_schema.routines WHERE LOWER(routine_name) LIKE LOWER(?) \
                 UNION ALL SELECT trigger_schema, trigger_name, 'TRIGGER' \
                 FROM information_schema.triggers WHERE LOWER(trigger_name) LIKE LOWER(?) \
                 ORDER BY 1, 2 LIMIT ?",
                (&pattern, &pattern, &pattern, limit as u64),
            )
            .await
            .map_err(query_error)?;
        Ok(rows
            .into_iter()
            .filter_map(|(db, name, ty)| {
                let kind = match ty.as_str() {
                    "BASE TABLE" | "TEMPORARY" => ObjectKind::Table,
                    "VIEW" | "SYSTEM VIEW" => ObjectKind::View,
                    "SEQUENCE" => ObjectKind::Sequence,
                    "PROCEDURE" => ObjectKind::Procedure,
                    "FUNCTION" => ObjectKind::Function,
                    "TRIGGER" => ObjectKind::Trigger,
                    _ => return None,
                };
                Some(ObjectRef::new(kind, name).in_database(db))
            })
            .collect())
    }

    async fn search_bodies(&mut self, term: &str, limit: usize) -> Result<Vec<ObjectRef>> {
        let pattern = format!("%{}%", like_escape(term));
        let rows: Vec<(String, String, String)> = self
            .conn()?
            .exec(
                "SELECT routine_schema, routine_name, routine_type \
                 FROM information_schema.routines \
                 WHERE LOWER(routine_definition) LIKE LOWER(?) \
                 ORDER BY 1, 2 LIMIT ?",
                (&pattern, limit as u64),
            )
            .await
            .map_err(query_error)?;
        Ok(rows
            .into_iter()
            .filter_map(|(db, name, ty)| {
                let kind = match ty.as_str() {
                    "PROCEDURE" => ObjectKind::Procedure,
                    "FUNCTION" => ObjectKind::Function,
                    _ => return None,
                };
                Some(ObjectRef::new(kind, name).in_database(db))
            })
            .collect())
    }

    async fn parameters(&mut self, routine: &ObjectRef) -> Result<Vec<RoutineParam>> {
        let db = self.database_or_current(&routine.database).await?;
        let routine_type = match routine.kind {
            ObjectKind::Procedure => "PROCEDURE",
            ObjectKind::Function => "FUNCTION",
            _ => return Err(DbError::Unsupported("parameters of this object")),
        };
        let rows: Vec<(Option<String>, Option<String>, String)> = self
            .conn()?
            .exec(
                // position 0 is a function's return value
                "SELECT parameter_name, parameter_mode, dtd_identifier \
                 FROM information_schema.parameters \
                 WHERE specific_schema = ? AND specific_name = ? AND routine_type = ? \
                   AND ordinal_position > 0 \
                 ORDER BY ordinal_position",
                (&db, &routine.name, routine_type),
            )
            .await
            .map_err(query_error)?;
        Ok(rows
            .into_iter()
            .map(|(name, mode, data_type)| RoutineParam {
                name: name.unwrap_or_default(),
                mode: match mode.as_deref() {
                    Some("OUT") => ParamMode::Out,
                    Some("INOUT") => ParamMode::InOut,
                    _ => ParamMode::In,
                },
                data_type,
            })
            .collect())
    }

    async fn definition(&mut self, object: &ObjectRef) -> Result<String> {
        let (show, column) = match object.kind {
            ObjectKind::Table => ("TABLE", 1),
            ObjectKind::View => ("VIEW", 1),
            ObjectKind::Procedure => ("PROCEDURE", 2),
            ObjectKind::Function => ("FUNCTION", 2),
            ObjectKind::Trigger => ("TRIGGER", 2),
            _ => return Err(DbError::Unsupported("definition for this object")),
        };
        let db = self.database_or_current(&object.database).await?;
        let sql = format!("SHOW CREATE {show} {}.{}", quote(&db), quote(&object.name));
        let row: Option<Row> = self.conn()?.query_first(sql).await.map_err(query_error)?;
        row.and_then(|r| r.get::<Option<String>, _>(column).flatten())
            .ok_or_else(|| {
                DbError::query(
                    "the server hides this definition from you; MySQL needs the \
                     SHOW_ROUTINE privilege for routines you didn't create",
                )
            })
    }

    async fn close(&mut self) -> Result<()> {
        match self.conn.take() {
            Some(conn) => conn.disconnect().await.map_err(query_error),
            None => Ok(()),
        }
    }
}

async fn explain_on(conn: &mut Conn, server: Server, text: &str, analyze: bool) -> Result<Plan> {
    let verb = if analyze {
        "EXPLAIN ANALYZE"
    } else {
        "EXPLAIN"
    };
    match server {
        Server::MariaDb => {
            let verb = if analyze { "ANALYZE" } else { "EXPLAIN" };
            let raw = plan_json(conn, &format!("{verb} FORMAT=JSON {text}")).await?;
            explain::parse_classic(&raw, analyze)
        }
        Server::MySql { tree: false } => {
            let raw = plan_json(conn, &format!("EXPLAIN FORMAT=JSON {text}")).await?;
            explain::parse_classic(&raw, false)
        }
        Server::MySql { tree: true } => {
            // the format is a session setting: put the user's back after
            let before: Option<String> = conn
                .query_first("SELECT @@explain_json_format_version")
                .await
                .map_err(query_error)?;
            let result = async {
                conn.query_drop("SET explain_json_format_version = 2")
                    .await
                    .map_err(query_error)?;
                let raw = plan_json(conn, &format!("{verb} FORMAT=JSON {text}")).await?;
                if !raw.contains(explain::NO_TREE) {
                    return explain::parse_tree(&raw, analyze);
                }
                // single-table UPDATE and DELETE only have the old format,
                // which has no actual figures
                conn.query_drop("SET explain_json_format_version = 1")
                    .await
                    .map_err(query_error)?;
                let raw = plan_json(conn, &format!("EXPLAIN FORMAT=JSON {text}")).await?;
                let mut plan = explain::parse_classic(&raw, false)?;
                if analyze {
                    plan.totals.push((
                        "Note".into(),
                        "MySQL can't analyze this statement; the plan is estimated.".into(),
                    ));
                }
                Ok(plan)
            }
            .await;
            if let Some(v) = before {
                let restored = conn
                    .query_drop(format!("SET explain_json_format_version = {}", v.trim()))
                    .await
                    .map_err(query_error);
                if result.is_ok() {
                    restored?;
                }
            }
            result
        }
    }
}

/// Tables the statement may touch whose engine can't roll back (MyISAM,
/// Aria, ...), as `schema.table (ENGINE)`. Goes by the names in the
/// statement and the tables in its plan, which covers views.
async fn non_transactional(conn: &mut Conn, server: Server, text: &str) -> Result<Vec<String>> {
    let candidates: Vec<(String, String, String)> = conn
        .query(
            "SELECT t.TABLE_SCHEMA, t.TABLE_NAME, t.ENGINE \
             FROM information_schema.TABLES t \
             JOIN information_schema.ENGINES e ON e.ENGINE = t.ENGINE \
             WHERE e.TRANSACTIONS <> 'YES' AND t.TABLE_SCHEMA NOT IN \
             ('mysql', 'information_schema', 'performance_schema', 'sys')",
        )
        .await
        .map_err(query_error)?;
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let mut named = explain::names(text);
    if let Ok(plan) = explain_on(conn, server, text, false).await {
        let mut found = Vec::new();
        explain::tables(&plan.root, &mut found);
        named.extend(found);
    }
    Ok(candidates
        .into_iter()
        .filter(|(_, table, _)| named.contains(&table.to_lowercase()))
        .map(|(schema, table, engine)| format!("{schema}.{table} ({engine})"))
        .collect())
}

async fn plan_json(conn: &mut Conn, sql: &str) -> Result<String> {
    let raw: Option<String> = conn.query_first(sql).await.map_err(query_error)?;
    raw.ok_or_else(|| DbError::Other("the server sent no plan".into()))
}

/// Runs one statement, which can still return several result sets (CALL),
/// and returns the server's warning count.
async fn run(
    conn: &mut Conn,
    sql: &str,
    sink: &mut ResultSink,
    cancelled: &AtomicBool,
) -> Result<u16> {
    let mut result = conn.query_iter(sql).await.map_err(query_error)?;
    loop {
        match result.columns().filter(|c| !c.is_empty()) {
            Some(columns) => {
                sink.columns(columns.iter().map(convert::column).collect())
                    .await?;
                while let Some(row) = result.next().await.map_err(query_error)? {
                    if cancelled.load(Ordering::SeqCst) {
                        return Err(DbError::Cancelled);
                    }
                    let row = row
                        .unwrap()
                        .into_iter()
                        .zip(columns.iter())
                        .map(|(v, c)| convert::value(c, v))
                        .collect();
                    sink.row(row).await?;
                }
                sink.done(None).await?;
            }
            None => {
                sink.done(Some(result.affected_rows())).await?;
                result.next().await.map_err(query_error)?;
            }
        }
        if result.is_empty() {
            return Ok(result.warnings());
        }
    }
}

async fn show_warnings(conn: &mut Conn, sink: &mut ResultSink) -> Result<()> {
    let rows: Vec<(String, u32, String)> =
        conn.query("SHOW WARNINGS").await.map_err(query_error)?;
    for (level, code, text) in rows {
        let level = match level.as_str() {
            "Note" => MessageLevel::Info,
            "Warning" => MessageLevel::Warning,
            _ => MessageLevel::Error,
        };
        sink.message(Message {
            level,
            text,
            code: Some(code.to_string()),
        })
        .await?;
    }
    Ok(())
}

struct MySqlCanceller {
    flag: Arc<AtomicBool>,
    opts: Opts,
    id: u32,
}

#[async_trait]
impl Canceller for MySqlCanceller {
    async fn cancel(&self) -> Result<()> {
        self.flag.store(true, Ordering::SeqCst);
        let mut conn = Conn::new(self.opts.clone()).await.map_err(connect_error)?;
        let killed = conn.query_drop(format!("KILL QUERY {}", self.id)).await;
        let _ = conn.disconnect().await;
        killed.map_err(query_error)
    }
}

/// The I/O errors behind a dropped connection read like library internals
/// ("peer closed connection without sending TLS close_notify: https://…").
fn lost_reason(io: &str) -> String {
    let closed = [
        "close_notify",
        "connection closed",
        "unexpected end of file",
        "Broken pipe",
        "Connection reset",
    ];
    if closed.iter().any(|c| io.contains(c)) {
        "the server closed the connection".into()
    } else {
        io.trim_start_matches("Input/output error: ").to_string()
    }
}

/// `term` matched literally inside LIKE.
fn like_escape(term: &str) -> String {
    let mut out = String::with_capacity(term.len());
    for c in term.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn quote(ident: &str) -> String {
    format!("`{}`", ident.replace('`', "``"))
}

fn ssl_opts(tls: &TlsConfig) -> Result<Option<SslOpts>> {
    if tls.mode == TlsMode::Disable {
        return Ok(None);
    }
    for path in [&tls.ca_file, &tls.cert_file, &tls.key_file]
        .into_iter()
        .flatten()
    {
        if !path.is_file() {
            return Err(DbError::Tls(format!("{} not found", path.display())));
        }
    }
    let verify = matches!(tls.mode, TlsMode::VerifyCa | TlsMode::VerifyFull);
    let mut opts = SslOpts::default()
        .with_danger_accept_invalid_certs(!verify)
        .with_danger_skip_domain_validation(tls.mode != TlsMode::VerifyFull);
    if let Some(ca) = &tls.ca_file {
        opts = opts.with_root_certs(vec![ca.clone().into()]);
    }
    match (&tls.cert_file, &tls.key_file) {
        (Some(cert), Some(key)) => {
            opts = opts.with_client_identity(Some(ClientIdentity::new(
                cert.clone().into(),
                key.clone().into(),
            )));
        }
        (None, None) => {}
        _ => {
            return Err(DbError::Tls(
                "a client certificate needs both the certificate and its key".into(),
            ))
        }
    }
    if let Some(name) = &tls.server_name {
        opts = opts.with_danger_tls_hostname_override(Some(name.clone()));
    }
    Ok(Some(opts))
}

fn connect_error(e: mysql_async::Error) -> DbError {
    match e {
        mysql_async::Error::Server(s) if AUTH_ERRORS.contains(&s.code) => DbError::Auth(s.message),
        mysql_async::Error::Server(s) => DbError::Connect(s.message),
        mysql_async::Error::Io(mysql_async::IoError::Tls(e)) => DbError::Tls(e.to_string()),
        mysql_async::Error::Driver(DriverError::NoClientSslFlagFromServer) => {
            DbError::Tls("the server doesn't offer TLS".into())
        }
        // rustls reports certificate problems as I/O errors
        mysql_async::Error::Io(e) if e.to_string().contains("certificate") => {
            let text = e.to_string();
            DbError::Tls(text.trim_start_matches("Input/output error: ").to_string())
        }
        other => DbError::Connect(other.to_string()),
    }
}

fn query_error(e: mysql_async::Error) -> DbError {
    match e {
        mysql_async::Error::Server(s) if s.code == ER_QUERY_INTERRUPTED => DbError::Cancelled,
        mysql_async::Error::Server(s) => DbError::Query {
            message: s.message,
            code: Some(s.code.to_string()),
            line: None,
        },
        mysql_async::Error::Io(e) => DbError::ConnectionLost(lost_reason(&e.to_string())),
        mysql_async::Error::Driver(DriverError::ConnectionClosed) => {
            DbError::ConnectionLost("server closed the connection".into())
        }
        other => DbError::Other(other.to_string()),
    }
}

/// Turns the statement-relative "at line N" of a server error into a line
/// in the whole script, or points at the statement when there's none.
fn at_line(e: DbError, statement_line: u32) -> DbError {
    match e {
        DbError::Query {
            message,
            code,
            line: None,
        } => {
            let relative = message
                .rsplit_once(" at line ")
                .and_then(|(_, n)| n.trim().parse::<u32>().ok())
                .unwrap_or(1);
            DbError::Query {
                message,
                code,
                line: Some(statement_line + relative.max(1) - 1),
            }
        }
        other => other,
    }
}

#[cfg(test)]
mod tests;
