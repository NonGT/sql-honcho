//! PostgreSQL, over tokio-postgres.
//!
//! Each statement is prepared first, for its column types, then run
//! through the simple query protocol, so every value comes back as the
//! text the server formats it in, whatever its type. A connection belongs
//! to one database: switching databases reconnects, and other databases'
//! metadata comes from side connections.

mod convert;
mod split;
mod tls;

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use futures_util::StreamExt;
use tokio::sync::mpsc;
use tokio_postgres::error::{ErrorPosition, SqlState};
use tokio_postgres::types::{ToSql, Type};
use tokio_postgres::{AsyncMessage, Client, SimpleQueryMessage};
use tokio_postgres_rustls::MakeRustlsConnect;
use tokio_util::sync::CancellationToken;

use crate::script::Statement;
use crate::types::*;
use crate::{Canceller, Connection, DbError, Driver, Result, ResultSink};

pub use split::split;

const DEFAULT_PORT: u16 = 5432;
// invalid_password, invalid_authorization_specification
const AUTH_ERRORS: &[&str] = &["28P01", "28000"];

/// Schemas that hold the user's objects rather than the server's.
const USER_SCHEMAS: &str =
    r"n.nspname NOT IN ('pg_catalog', 'information_schema') AND n.nspname NOT LIKE 'pg\_%'";

pub struct PostgresDriver;

#[async_trait]
impl Driver for PostgresDriver {
    fn kind(&self) -> DriverKind {
        DriverKind::Postgres
    }

    fn default_port(&self) -> Option<u16> {
        Some(DEFAULT_PORT)
    }

    async fn connect(&self, config: &ConnectConfig) -> Result<Box<dyn Connection>> {
        crate::runtime::install_crypto_provider();
        let database = match &config.database {
            Some(db) if !db.is_empty() => db.clone(),
            // the server's own default
            _ if !config.user.is_empty() => config.user.clone(),
            _ => "postgres".into(),
        };
        let main = Pg::open(config, &database).await?;
        Ok(Box::new(PostgresConnection {
            config: config.clone(),
            database,
            main,
            side: HashMap::new(),
            cancel: Mutex::default(),
        }))
    }
}

/// One connection to the server, with the notices it has sent.
struct Pg {
    client: Client,
    notices: mpsc::UnboundedReceiver<Message>,
    tls: MakeRustlsConnect,
}

impl Pg {
    async fn open(config: &ConnectConfig, database: &str) -> Result<Pg> {
        let (tls, mode) = tls::connector(&config.tls)?;
        let mut pg = tokio_postgres::Config::new();
        pg.host(match config.host.as_str() {
            "" => "localhost",
            h => h,
        })
        .port(config.port.unwrap_or(DEFAULT_PORT))
        .user(&config.user)
        .dbname(database)
        .application_name("sql-honcho")
        .ssl_mode(mode);
        if let Some(password) = &config.password {
            pg.password(password);
        }
        if let Some(limit) = config.connect_timeout {
            pg.connect_timeout(limit);
        }
        if config.read_only {
            pg.options("-c default_transaction_read_only=on");
        }
        let (client, mut connection) = pg.connect(tls.clone()).await.map_err(connect_error)?;
        let (tx, notices) = mpsc::unbounded_channel();
        // drives the connection; notices are passed on before any later
        // reply reaches the client, so they stay in order
        tokio::spawn(async move {
            let mut messages = futures_util::stream::poll_fn(|cx| connection.poll_message(cx));
            while let Some(Ok(message)) = messages.next().await {
                if let AsyncMessage::Notice(n) = message {
                    let _ = tx.send(notice(&n));
                }
            }
        });
        Ok(Pg {
            client,
            notices,
            tls,
        })
    }
}

fn notice(n: &tokio_postgres::error::DbError) -> Message {
    let mut text = n.message().to_string();
    if let Some(detail) = n.detail() {
        text = format!("{text}\n{detail}");
    }
    Message {
        level: match n.severity() {
            "WARNING" => MessageLevel::Warning,
            _ => MessageLevel::Info,
        },
        text,
        code: Some(n.code().code().to_string()),
    }
}

pub struct PostgresConnection {
    config: ConnectConfig,
    database: String,
    main: Pg,
    /// Other databases, for listing what's in them.
    side: HashMap<String, Client>,
    cancel: Mutex<CancellationToken>,
}

impl PostgresConnection {
    fn client(&self) -> Result<&Client> {
        if self.main.client.is_closed() {
            return Err(DbError::ConnectionLost(
                "the server closed the connection".into(),
            ));
        }
        Ok(&self.main.client)
    }

    /// A connection to `database` for reading its catalog.
    async fn meta(&mut self, database: Option<&str>) -> Result<&Client> {
        let db = match database {
            Some(db) if db != self.database => db.to_string(),
            _ => return self.client(),
        };
        if self.side.get(&db).is_none_or(Client::is_closed) {
            let pg = Pg::open(&self.config, &db).await?;
            self.side.insert(db.clone(), pg.client);
        }
        Ok(&self.side[&db])
    }

    async fn rows(
        &mut self,
        database: Option<&str>,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Vec<tokio_postgres::Row>> {
        self.meta(database)
            .await?
            .query(sql, params)
            .await
            .map_err(plain_error)
    }

    async fn relation(&mut self, o: &ObjectRef) -> Result<u32> {
        let rows = self
            .rows(
                o.database.as_deref(),
                "SELECT c.oid FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE c.relname = $1 AND ($2::text IS NULL OR n.nspname = $2) \
                 ORDER BY array_position(current_schemas(true), n.nspname::text) NULLS LAST \
                 LIMIT 1",
                &[&o.name, &o.schema],
            )
            .await?;
        rows.first()
            .map(|r| r.get(0))
            .ok_or_else(|| DbError::query(format!("no table or view {}", o.name)))
    }

    /// The routine, telling overloads apart by their arguments.
    async fn routine(&mut self, o: &ObjectRef) -> Result<u32> {
        let args = o
            .signature
            .as_deref()
            .map(|s| s.split(" → ").next().unwrap_or(s).to_string());
        let rows = self
            .rows(
                o.database.as_deref(),
                "SELECT p.oid FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
                 WHERE p.proname = $1 AND ($2::text IS NULL OR n.nspname = $2) \
                   AND ($3::text IS NULL \
                     OR '(' || pg_get_function_identity_arguments(p.oid) || ')' = $3) \
                 ORDER BY array_position(current_schemas(true), n.nspname::text) NULLS LAST \
                 LIMIT 1",
                &[&o.name, &o.schema, &args],
            )
            .await?;
        rows.first()
            .map(|r| r.get(0))
            .ok_or_else(|| DbError::query(format!("no routine {}", o.name)))
    }

    async fn trigger(&mut self, o: &ObjectRef) -> Result<u32> {
        let rows = self
            .rows(
                o.database.as_deref(),
                "SELECT t.oid FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE t.tgname = $1 AND ($2::text IS NULL OR n.nspname = $2) \
                   AND NOT t.tgisinternal LIMIT 1",
                &[&o.name, &o.schema],
            )
            .await?;
        rows.first()
            .map(|r| r.get(0))
            .ok_or_else(|| DbError::query(format!("no trigger {}", o.name)))
    }

    async fn table_definition(&mut self, o: &ObjectRef) -> Result<String> {
        let oid = self.relation(o).await?;
        let db = o.database.as_deref();
        let name: String = self
            .rows(
                db,
                "SELECT quote_ident(n.nspname) || '.' || quote_ident(c.relname) \
                 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE c.oid = $1",
                &[&oid],
            )
            .await?[0]
            .get(0);
        let columns = self
            .rows(
                db,
                "SELECT quote_ident(a.attname), format_type(a.atttypid, a.atttypmod), \
                 a.attnotnull, pg_get_expr(d.adbin, d.adrelid), a.attidentity::text, \
                 a.attgenerated::text \
                 FROM pg_attribute a \
                 LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
                 WHERE a.attrelid = $1 AND a.attnum > 0 AND NOT a.attisdropped \
                 ORDER BY a.attnum",
                &[&oid],
            )
            .await?;
        let mut lines: Vec<String> = columns
            .iter()
            .map(|r| {
                let mut line = format!("    {} {}", r.get::<_, String>(0), r.get::<_, String>(1));
                let default: Option<String> = r.get(3);
                match (
                    r.get::<_, String>(4).as_str(),
                    r.get::<_, String>(5).as_str(),
                ) {
                    ("a", _) => line += " GENERATED ALWAYS AS IDENTITY",
                    ("d", _) => line += " GENERATED BY DEFAULT AS IDENTITY",
                    (_, "s") => {
                        line += &format!(
                            " GENERATED ALWAYS AS ({}) STORED",
                            default.unwrap_or_default()
                        )
                    }
                    _ => {
                        if let Some(default) = default {
                            line += &format!(" DEFAULT {default}");
                        }
                    }
                }
                if r.get::<_, bool>(2) {
                    line += " NOT NULL";
                }
                line
            })
            .collect();
        let constraints = self
            .rows(
                db,
                "SELECT quote_ident(conname), pg_get_constraintdef(oid, true) FROM pg_constraint \
                 WHERE conrelid = $1 AND contype IN ('p', 'u', 'f', 'c', 'x') \
                 ORDER BY contype = 'p' DESC, conname",
                &[&oid],
            )
            .await?;
        lines.extend(constraints.iter().map(|r| {
            format!(
                "    CONSTRAINT {} {}",
                r.get::<_, String>(0),
                r.get::<_, String>(1)
            )
        }));
        let mut out = format!("CREATE TABLE {name} (\n{}\n);", lines.join(",\n"));
        // indexes that aren't behind a constraint
        let indexes = self
            .rows(
                db,
                "SELECT pg_get_indexdef(i.indexrelid) FROM pg_index i \
                 WHERE i.indrelid = $1 AND NOT EXISTS \
                   (SELECT 1 FROM pg_constraint c WHERE c.conindid = i.indexrelid) \
                 ORDER BY 1",
                &[&oid],
            )
            .await?;
        for r in &indexes {
            out += &format!("\n{};", r.get::<_, String>(0));
        }
        Ok(out)
    }

    async fn search_each_database(
        &mut self,
        sql: &str,
        term: &str,
        limit: usize,
    ) -> Result<Vec<ObjectRef>> {
        let mut out = Vec::new();
        for db in self.databases().await? {
            let left = limit.saturating_sub(out.len());
            if left == 0 {
                break;
            }
            let rows = match self.rows(Some(&db), sql, &[&term, &(left as i64)]).await {
                Ok(rows) => rows,
                Err(e) if e.is_fatal() => return Err(e),
                // a database closed to this login, say
                Err(_) => continue,
            };
            out.extend(rows.iter().filter_map(|r| object(r, &db)));
        }
        Ok(out)
    }
}

/// Tables, views, sequences, routines and triggers in user schemas, as
/// (schema, name, kind, signature). `filter` narrows each part further.
fn objects_sql(filter: &str) -> String {
    format!(
        "SELECT n.nspname::text AS schema, c.relname::text AS name, \
           CASE WHEN c.relkind IN ('r', 'p') THEN 'table' WHEN c.relkind = 'v' THEN 'view' \
             WHEN c.relkind = 'm' THEN 'matview' ELSE 'sequence' END AS kind, \
           NULL::text AS signature \
         FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE c.relkind IN ('r', 'p', 'v', 'm', 'S') AND NOT c.relispartition \
           AND {USER_SCHEMAS} {filter} \
         UNION ALL \
         SELECT n.nspname::text, p.proname::text, \
           CASE p.prokind WHEN 'p' THEN 'procedure' ELSE 'function' END, \
           '(' || pg_get_function_identity_arguments(p.oid) || ')' || \
             CASE WHEN p.prokind = 'p' THEN '' ELSE ' → ' || pg_get_function_result(p.oid) END \
         FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
         WHERE p.prokind IN ('f', 'p') AND {USER_SCHEMAS} {filter_p} \
           AND NOT EXISTS (SELECT 1 FROM pg_depend d \
             WHERE d.classid = 'pg_proc'::regclass AND d.objid = p.oid AND d.deptype = 'e') \
         UNION ALL \
         SELECT n.nspname::text, t.tgname::text, 'trigger', NULL \
         FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE NOT t.tgisinternal AND {USER_SCHEMAS} {filter_t}",
        filter = filter.replace("{name}", "c.relname"),
        filter_p = filter.replace("{name}", "p.proname"),
        filter_t = filter.replace("{name}", "t.tgname"),
    )
}

fn object(r: &tokio_postgres::Row, database: &str) -> Option<ObjectRef> {
    let kind = match r.get::<_, String>(2).as_str() {
        "table" => ObjectKind::Table,
        "view" => ObjectKind::View,
        "matview" => ObjectKind::MaterializedView,
        "sequence" => ObjectKind::Sequence,
        "procedure" => ObjectKind::Procedure,
        "function" => ObjectKind::Function,
        "trigger" => ObjectKind::Trigger,
        _ => return None,
    };
    Some(ObjectRef {
        signature: r.get(3),
        ..ObjectRef::new(kind, r.get::<_, String>(1))
            .in_database(database)
            .in_schema(r.get::<_, String>(0))
    })
}

#[async_trait]
impl Connection for PostgresConnection {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            schemas: true,
            cross_database: false,
        }
    }

    async fn execute(&mut self, sql: &str, sink: &mut ResultSink) -> Result<()> {
        let token = self.cancel.lock().unwrap().clone();
        self.client()?;
        let result = async {
            for statement in split(sql) {
                run(&mut self.main, &statement, sink, &token).await?;
            }
            Ok(())
        }
        .await;
        // notices from the last statement, or from the one that failed
        pass_notices(&mut self.main.notices, sink).await?;
        result
    }

    fn statements<'a>(&self, sql: &'a str) -> Vec<Statement<'a>> {
        split(sql)
    }

    fn canceller(&self) -> Option<Box<dyn Canceller>> {
        let token = CancellationToken::new();
        *self.cancel.lock().unwrap() = token.clone();
        Some(Box::new(PostgresCanceller(token)))
    }

    async fn ping(&mut self) -> Result<()> {
        self.client()?
            .simple_query("SELECT 1")
            .await
            .map(drop)
            .map_err(plain_error)
    }

    async fn current_database(&mut self) -> Result<Option<String>> {
        Ok(Some(self.database.clone()))
    }

    async fn in_transaction(&mut self) -> Result<bool> {
        // outside a transaction block, each statement's transaction starts
        // with it; inside one, now() is when BEGIN ran. A simple query, as
        // the extended protocol moves statement_timestamp() on at Execute.
        let sql = "SELECT now() <> statement_timestamp()";
        match self.client()?.simple_query(sql).await {
            Ok(messages) => Ok(messages
                .iter()
                .any(|m| matches!(m, SimpleQueryMessage::Row(r) if r.get(0) == Some("t")))),
            // open, and waiting for a ROLLBACK
            Err(e) if e.code() == Some(&SqlState::IN_FAILED_SQL_TRANSACTION) => Ok(true),
            Err(e) => Err(plain_error(e)),
        }
    }

    async fn use_database(&mut self, name: &str) -> Result<()> {
        if name == self.database {
            return Ok(());
        }
        // a connection is bound to its database: open one to the other
        self.main = Pg::open(&self.config, name).await?;
        self.side.remove(name);
        self.database = name.to_string();
        Ok(())
    }

    async fn databases(&mut self) -> Result<Vec<String>> {
        let rows = self
            .rows(
                None,
                "SELECT datname::text FROM pg_database \
                 WHERE datallowconn AND NOT datistemplate \
                   AND has_database_privilege(datname, 'CONNECT') \
                 ORDER BY datname",
                &[],
            )
            .await?;
        Ok(rows.iter().map(|r| r.get(0)).collect())
    }

    async fn schemas(&mut self, database: &str) -> Result<Vec<String>> {
        let sql =
            format!("SELECT n.nspname::text FROM pg_namespace n WHERE {USER_SCHEMAS} ORDER BY 1");
        let rows = self.rows(Some(database), &sql, &[]).await?;
        Ok(rows.iter().map(|r| r.get(0)).collect())
    }

    async fn objects(&mut self, scope: &Scope, kinds: &[ObjectKind]) -> Result<Vec<ObjectRef>> {
        let db = scope
            .database
            .clone()
            .unwrap_or_else(|| self.database.clone());
        let rows = match &scope.schema {
            Some(schema) => {
                let sql = format!("{} ORDER BY 1, 2", objects_sql("AND n.nspname = $1"));
                self.rows(Some(&db), &sql, &[schema]).await?
            }
            None => {
                let sql = format!("{} ORDER BY 1, 2", objects_sql(""));
                self.rows(Some(&db), &sql, &[]).await?
            }
        };
        Ok(rows
            .iter()
            .filter_map(|r| object(r, &db))
            .filter(|o| kinds.is_empty() || kinds.contains(&o.kind))
            .collect())
    }

    async fn columns(&mut self, o: &ObjectRef) -> Result<Vec<ColumnInfo>> {
        let oid = self.relation(o).await?;
        let rows = self
            .rows(
                o.database.as_deref(),
                "SELECT a.attname::text, format_type(a.atttypid, a.atttypmod), NOT a.attnotnull, \
                 pg_get_expr(d.adbin, d.adrelid), \
                 EXISTS (SELECT 1 FROM pg_index i WHERE i.indrelid = a.attrelid \
                   AND i.indisprimary AND a.attnum = ANY (i.indkey)), \
                 col_description(a.attrelid, a.attnum) \
                 FROM pg_attribute a \
                 LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
                 WHERE a.attrelid = $1 AND a.attnum > 0 AND NOT a.attisdropped \
                 ORDER BY a.attnum",
                &[&oid],
            )
            .await?;
        Ok(rows
            .iter()
            .map(|r| ColumnInfo {
                name: r.get(0),
                data_type: r.get(1),
                nullable: r.get(2),
                default: r.get(3),
                primary_key: r.get(4),
                comment: r.get(5),
            })
            .collect())
    }

    async fn indexes(&mut self, table: &ObjectRef) -> Result<Vec<IndexInfo>> {
        let oid = self.relation(table).await?;
        let rows = self
            .rows(
                table.database.as_deref(),
                "SELECT ic.relname::text, i.indisunique, i.indisprimary, \
                   coalesce(a.attname::text, '(expression)') \
                 FROM pg_index i JOIN pg_class ic ON ic.oid = i.indexrelid \
                 CROSS JOIN LATERAL unnest(i.indkey::int2[]) WITH ORDINALITY AS k(attnum, ord) \
                 LEFT JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = k.attnum \
                 WHERE i.indrelid = $1 AND k.ord <= i.indnkeyatts \
                 ORDER BY i.indisprimary DESC, ic.relname, k.ord",
                &[&oid],
            )
            .await?;
        let mut out: Vec<IndexInfo> = Vec::new();
        for r in &rows {
            let name: String = r.get(0);
            let column: String = r.get(3);
            match out.last_mut() {
                Some(last) if last.name == name => last.columns.push(column),
                _ => out.push(IndexInfo {
                    name,
                    columns: vec![column],
                    unique: r.get(1),
                    primary: r.get(2),
                }),
            }
        }
        Ok(out)
    }

    async fn search(&mut self, term: &str, limit: usize) -> Result<Vec<ObjectRef>> {
        let sql = format!(
            "{} ORDER BY 1, 2 LIMIT $2",
            objects_sql("AND strpos(lower({name}), lower($1)) > 0")
        );
        self.search_each_database(&sql, term, limit).await
    }

    async fn search_bodies(&mut self, term: &str, limit: usize) -> Result<Vec<ObjectRef>> {
        let sql = format!(
            "SELECT n.nspname::text, p.proname::text, \
               CASE p.prokind WHEN 'p' THEN 'procedure' ELSE 'function' END, \
               '(' || pg_get_function_identity_arguments(p.oid) || ')' || \
                 CASE WHEN p.prokind = 'p' THEN '' ELSE ' → ' || pg_get_function_result(p.oid) END \
             FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
             WHERE p.prokind IN ('f', 'p') AND {USER_SCHEMAS} \
               AND strpos(lower(p.prosrc), lower($1)) > 0 \
             ORDER BY 1, 2 LIMIT $2"
        );
        self.search_each_database(&sql, term, limit).await
    }

    async fn parameters(&mut self, routine: &ObjectRef) -> Result<Vec<RoutineParam>> {
        if !routine.kind.is_routine() {
            return Err(DbError::Unsupported("parameters of this object"));
        }
        let oid = self.routine(routine).await?;
        let rows = self
            .rows(
                routine.database.as_deref(),
                "SELECT coalesce(p.proargnames[u.i], ''), coalesce(p.proargmodes[u.i]::text, 'i'), \
                   format_type(u.t, NULL) \
                 FROM pg_proc p, \
                   unnest(coalesce(p.proallargtypes, p.proargtypes::oid[])) WITH ORDINALITY AS u(t, i) \
                 WHERE p.oid = $1 ORDER BY u.i",
                &[&oid],
            )
            .await?;
        let function = routine.kind == ObjectKind::Function;
        Ok(rows
            .iter()
            .filter_map(|r| {
                let mode = match r.get::<_, String>(1).as_str() {
                    "i" | "v" => ParamMode::In,
                    "b" => ParamMode::InOut,
                    // a function's OUT parameters are its result columns
                    "o" if !function => ParamMode::Out,
                    _ => return None,
                };
                Some(RoutineParam {
                    name: r.get(0),
                    mode,
                    data_type: r.get(2),
                })
            })
            .collect())
    }

    async fn definition(&mut self, o: &ObjectRef) -> Result<String> {
        let db = o.database.clone();
        let one = |rows: Vec<tokio_postgres::Row>| -> Result<String> {
            rows.first()
                .and_then(|r| r.get::<_, Option<String>>(0))
                .ok_or_else(|| DbError::query(format!("no definition for {}", o.name)))
        };
        match o.kind {
            ObjectKind::Table => self.table_definition(o).await,
            ObjectKind::View | ObjectKind::MaterializedView => {
                let oid = self.relation(o).await?;
                let create = if o.kind == ObjectKind::View {
                    "CREATE OR REPLACE VIEW"
                } else {
                    "CREATE MATERIALIZED VIEW"
                };
                let rows = self
                    .rows(
                        db.as_deref(),
                        &format!(
                            "SELECT '{create} ' || quote_ident(n.nspname) || '.' || \
                               quote_ident(c.relname) || E' AS\\n' || pg_get_viewdef(c.oid, true) \
                             FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
                             WHERE c.oid = $1"
                        ),
                        &[&oid],
                    )
                    .await?;
                one(rows)
            }
            ObjectKind::Procedure | ObjectKind::Function => {
                let oid = self.routine(o).await?;
                let rows = self
                    .rows(db.as_deref(), "SELECT pg_get_functiondef($1)", &[&oid])
                    .await?;
                one(rows)
            }
            ObjectKind::Trigger => {
                let oid = self.trigger(o).await?;
                let rows = self
                    .rows(
                        db.as_deref(),
                        "SELECT pg_get_triggerdef(oid, true) || ';' FROM pg_trigger WHERE oid = $1",
                        &[&oid],
                    )
                    .await?;
                one(rows)
            }
            ObjectKind::Sequence => {
                let oid = self.relation(o).await?;
                let rows = self
                    .rows(
                        db.as_deref(),
                        "SELECT format('CREATE SEQUENCE %I.%I AS %s INCREMENT BY %s MINVALUE %s \
                           MAXVALUE %s START WITH %s CACHE %s%s;', s.schemaname, s.sequencename, \
                           s.data_type, s.increment_by, s.min_value, s.max_value, s.start_value, \
                           s.cache_size, CASE WHEN s.cycle THEN ' CYCLE' ELSE '' END) \
                         FROM pg_sequences s JOIN pg_class c ON c.relname = s.sequencename \
                         JOIN pg_namespace n ON n.oid = c.relnamespace AND n.nspname = s.schemaname \
                         WHERE c.oid = $1",
                        &[&oid],
                    )
                    .await?;
                one(rows)
            }
        }
    }
}

/// Runs one statement: prepared for its column types, then run as a
/// simple query for its rows as text.
async fn run(
    pg: &mut Pg,
    statement: &Statement<'_>,
    sink: &mut ResultSink,
    token: &CancellationToken,
) -> Result<()> {
    let prepared = tokio::select! {
        biased;
        _ = token.cancelled() => return Err(DbError::Cancelled),
        prepared = pg.client.prepare(statement.text) => prepared,
    };
    pass_notices(&mut pg.notices, sink).await?;
    let prepared = prepared.map_err(|e| statement_error(e, statement))?;
    let types: Vec<Type> = prepared
        .columns()
        .iter()
        .map(|c| c.type_().clone())
        .collect();
    let has_rows = !types.is_empty();
    if has_rows {
        sink.columns(convert::columns(prepared.columns())).await?;
    }
    let stream = pg
        .client
        .simple_query_raw(statement.text)
        .await
        .map_err(|e| statement_error(e, statement))?;
    futures_util::pin_mut!(stream);
    let mut cancelled = false;
    loop {
        let item = tokio::select! {
            biased;
            _ = token.cancelled(), if !cancelled => {
                cancelled = true;
                let _ = pg.client.cancel_token().cancel_query(pg.tls.clone()).await;
                continue;
            }
            item = stream.next() => item,
        };
        let message = match item {
            None => break,
            // the server reports the cancel as an error
            Some(Err(_)) if cancelled => return Err(DbError::Cancelled),
            Some(Err(e)) => return Err(statement_error(e, statement)),
            Some(Ok(_)) if cancelled => continue,
            Some(Ok(message)) => message,
        };
        let sent = async {
            pass_notices(&mut pg.notices, sink).await?;
            match message {
                SimpleQueryMessage::Row(row) => {
                    let values = (0..row.len())
                        .map(|i| convert::value(types.get(i).unwrap_or(&Type::TEXT), row.get(i)))
                        .collect();
                    sink.row(values).await
                }
                SimpleQueryMessage::CommandComplete(n) => {
                    sink.done((!has_rows && counts_rows(statement.text)).then_some(n))
                        .await
                }
                _ => Ok(()),
            }
        }
        .await;
        if let Err(e) = sent {
            // nobody is reading any more: stop the server too
            let _ = pg.client.cancel_token().cancel_query(pg.tls.clone()).await;
            return Err(e);
        }
    }
    if cancelled {
        return Err(DbError::Cancelled);
    }
    Ok(())
}

/// Whether the statement's count is rows it changed, rather than nothing
/// (CREATE, SET, ...).
fn counts_rows(sql: &str) -> bool {
    let code = &sql[crate::routine::code_start(sql)..];
    let first = code
        .split(|c: char| !c.is_ascii_alphabetic())
        .next()
        .unwrap_or("");
    ["INSERT", "UPDATE", "DELETE", "MERGE", "COPY", "WITH"]
        .iter()
        .any(|w| w.eq_ignore_ascii_case(first))
}

async fn pass_notices(
    notices: &mut mpsc::UnboundedReceiver<Message>,
    sink: &mut ResultSink,
) -> Result<()> {
    while let Ok(m) = notices.try_recv() {
        sink.message(m).await?;
    }
    Ok(())
}

struct PostgresCanceller(CancellationToken);

#[async_trait]
impl Canceller for PostgresCanceller {
    async fn cancel(&self) -> Result<()> {
        self.0.cancel();
        Ok(())
    }
}

fn lost(e: &tokio_postgres::Error) -> bool {
    e.is_closed() || std::error::Error::source(e).is_some_and(|s| s.is::<std::io::Error>())
}

/// An error in a statement of the script, its position turned into a
/// line of the whole script.
fn statement_error(e: tokio_postgres::Error, statement: &Statement<'_>) -> DbError {
    if lost(&e) {
        return DbError::ConnectionLost(e.to_string());
    }
    let Some(db) = e.as_db_error() else {
        return DbError::Other(e.to_string());
    };
    let line = match db.position() {
        Some(ErrorPosition::Original(at)) => {
            let before: String = statement
                .text
                .chars()
                .take((*at as usize).saturating_sub(1))
                .collect();
            statement.line + before.matches('\n').count() as u32
        }
        _ => statement.line,
    };
    DbError::Query {
        message: server_message(db),
        code: Some(db.code().code().to_string()),
        line: Some(line),
    }
}

fn server_message(db: &tokio_postgres::error::DbError) -> String {
    let mut message = db.message().to_string();
    if let Some(detail) = db.detail() {
        message = format!("{message}\n{detail}");
    }
    if let Some(hint) = db.hint() {
        message = format!("{message}\nHint: {hint}");
    }
    message
}

fn plain_error(e: tokio_postgres::Error) -> DbError {
    if lost(&e) {
        return DbError::ConnectionLost(e.to_string());
    }
    match e.as_db_error() {
        Some(db) => DbError::Query {
            message: server_message(db),
            code: Some(db.code().code().to_string()),
            line: None,
        },
        None => DbError::Other(e.to_string()),
    }
}

fn connect_error(e: tokio_postgres::Error) -> DbError {
    if let Some(db) = e.as_db_error() {
        let message = db.message().to_string();
        return if AUTH_ERRORS.contains(&db.code().code()) {
            DbError::Auth(message)
        } else {
            DbError::Connect(message)
        };
    }
    // the cause carries the useful part (refused, certificate, ...)
    let mut text = e.to_string();
    if let Some(source) = std::error::Error::source(&e) {
        text = format!("{text}: {source}");
    }
    if text.contains("TLS") || text.contains("certificate") {
        DbError::Tls(text)
    } else {
        DbError::Connect(text)
    }
}

#[cfg(test)]
mod tests;
