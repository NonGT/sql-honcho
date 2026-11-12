//! SQL Server and Azure SQL, over tiberius.

mod convert;
mod explain;
mod split;

use std::sync::Mutex;

use async_trait::async_trait;
use futures_util::StreamExt;
use tiberius::{AuthMethod, BatchItem, ColumnType, EncryptionLevel, Row, SqlBrowser, ToSql};
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};
use tokio_util::sync::CancellationToken;

use crate::script::Statement;
use crate::types::*;
use crate::{Canceller, Connection, DbError, Driver, Plan, Result, ResultSink};

pub use split::split;

type Client = tiberius::Client<Compat<TcpStream>>;

const DEFAULT_PORT: u16 = 1433;
const AUTH_ERRORS: &[u32] = &[
    18456, // login failed
    18470, // account disabled
    18486, // account locked out
    18487, // password expired
    18488, // password must be changed
];
// "Changed database context to ..." and its language twin, sent for every USE
const CONTEXT_NOTICES: &[u32] = &[5701, 5703];

// sys.objects types the browser shows
const OBJECT_TYPES: &str = "'U', 'V', 'P', 'PC', 'FN', 'IF', 'TF', 'FS', 'FT', 'TR', 'SO'";
const ROUTINE_TYPES: &str = "'P', 'PC', 'FN', 'IF', 'TF', 'FS', 'FT'";

pub struct SqlServerDriver;

#[async_trait]
impl Driver for SqlServerDriver {
    fn kind(&self) -> DriverKind {
        DriverKind::SqlServer
    }

    fn default_port(&self) -> Option<u16> {
        Some(DEFAULT_PORT)
    }

    async fn connect(&self, config: &ConnectConfig) -> Result<Box<dyn Connection>> {
        crate::runtime::install_crypto_provider();
        let (tds, named) = tds_config(config)?;
        let client = match config.connect_timeout {
            Some(limit) => tokio::time::timeout(limit, open(tds, named))
                .await
                .map_err(|_| DbError::Connect("timed out".into()))??,
            None => open(tds, named).await?,
        };
        Ok(Box::new(SqlServerConnection {
            client: Some(client),
            cancel: Mutex::default(),
        }))
    }
}

/// The tiberius config, and whether to find the port through SQL Browser
/// (a named instance without an explicit port).
fn tds_config(config: &ConnectConfig) -> Result<(tiberius::Config, bool)> {
    let mut tds = tiberius::Config::new();
    let host = match config.host.as_str() {
        "" => "localhost",
        h => h,
    };
    let mut named = false;
    match host.split_once('\\') {
        Some((host, instance)) => {
            tds.host(host);
            tds.instance_name(instance);
            named = config.port.is_none();
        }
        None => tds.host(host),
    }
    tds.port(config.port.unwrap_or(DEFAULT_PORT));
    if let Some(db) = &config.database {
        tds.database(db);
    }
    tds.application_name("sql-honcho");
    tds.authentication(auth(config)?);
    tls(&mut tds, &config.tls)?;
    Ok((tds, named))
}

fn auth(config: &ConnectConfig) -> Result<AuthMethod> {
    let password = config.password.clone().unwrap_or_default();
    match config.options.get("auth").map(String::as_str) {
        None | Some("sql") => Ok(AuthMethod::sql_server(&config.user, password)),
        // a token from `az account get-access-token`, say, in the password field
        Some("aad") => Ok(AuthMethod::aad_token(password)),
        #[cfg(windows)]
        Some("windows") if config.user.is_empty() => Ok(AuthMethod::Integrated),
        #[cfg(windows)]
        Some("windows") => Ok(AuthMethod::windows(&config.user, password)),
        #[cfg(not(windows))]
        Some("windows") => Err(DbError::Connect(
            "Windows authentication only works on Windows".into(),
        )),
        Some(other) => Err(DbError::Connect(format!(
            "unknown authentication method {other}"
        ))),
    }
}

fn tls(tds: &mut tiberius::Config, tls: &TlsConfig) -> Result<()> {
    for path in [&tls.ca_file, &tls.cert_file, &tls.key_file]
        .into_iter()
        .flatten()
    {
        if !path.is_file() {
            return Err(DbError::Tls(format!("{} not found", path.display())));
        }
    }
    tds.encryption(match tls.mode {
        TlsMode::Disable => EncryptionLevel::NotSupported,
        // On settles for plain TCP when the server has no TLS
        TlsMode::Prefer => EncryptionLevel::On,
        TlsMode::Require | TlsMode::VerifyCa | TlsMode::VerifyFull => EncryptionLevel::Required,
    });
    // tiberius always checks the host name once it checks the chain, so
    // verify-ca behaves like verify-full
    match tls.mode {
        TlsMode::VerifyCa | TlsMode::VerifyFull => {
            if let Some(ca) = &tls.ca_file {
                tds.trust_cert_ca(ca.display());
            }
            if let Some(name) = &tls.server_name {
                tds.hostname_in_certificate(name);
            }
        }
        _ => tds.trust_cert(),
    }
    match (&tls.cert_file, &tls.key_file) {
        (Some(cert), Some(key)) => tds.client_certificate(cert, key),
        (None, None) => {}
        _ => {
            return Err(DbError::Tls(
                "a client certificate needs both the certificate and its key".into(),
            ))
        }
    }
    Ok(())
}

async fn open(mut tds: tiberius::Config, named: bool) -> Result<Client> {
    // Azure SQL can redirect the login to another node, once
    for _ in 0..2 {
        let tcp = if named {
            TcpStream::connect_named(&tds)
                .await
                .map_err(connect_error)?
        } else {
            TcpStream::connect(tds.get_addr())
                .await
                .map_err(|e| DbError::Connect(e.to_string()))?
        };
        tcp.set_nodelay(true).ok();
        match Client::connect(tds.clone(), tcp.compat_write()).await {
            Ok(client) => return Ok(client),
            Err(tiberius::error::Error::Routing { host, port }) => {
                tds.host(host);
                tds.port(port);
            }
            Err(e) => return Err(connect_error(e)),
        }
    }
    Err(DbError::Connect(
        "the server redirected the login twice".into(),
    ))
}

pub struct SqlServerConnection {
    client: Option<Client>,
    /// Fired by the canceller handed out for the current run.
    cancel: Mutex<CancellationToken>,
}

impl SqlServerConnection {
    fn client(&mut self) -> Result<&mut Client> {
        self.client
            .as_mut()
            .ok_or_else(|| DbError::ConnectionLost("connection closed".into()))
    }

    async fn query(&mut self, sql: &str, params: &[&dyn ToSql]) -> Result<Vec<Row>> {
        let stream = self
            .client()?
            .query(sql, params)
            .await
            .map_err(query_error)?;
        stream.into_first_result().await.map_err(query_error)
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

    /// `[db].[schema].[name]`, for OBJECT_ID.
    async fn full_name(&mut self, object: &ObjectRef) -> Result<String> {
        let db = self.database_or_current(&object.database).await?;
        let schema = object.schema.as_deref().unwrap_or("dbo");
        Ok(format!(
            "{}.{}.{}",
            quote(&db),
            quote(schema),
            quote(&object.name)
        ))
    }

    async fn table_definition(&mut self, table: &ObjectRef) -> Result<String> {
        let name = self.full_name(table).await?;
        let db = quote(&self.database_or_current(&table.database).await?);
        let rows = self
            .query(
                &format!(
                    "SELECT c.name, {ty}, c.is_nullable, \
                     CAST(c.is_identity AS bit), \
                     CAST(ic.seed_value AS nvarchar(40)), CAST(ic.increment_value AS nvarchar(40)), \
                     dc.definition \
                     FROM {db}.sys.columns c \
                     JOIN {db}.sys.types t ON t.user_type_id = c.user_type_id \
                     LEFT JOIN {db}.sys.identity_columns ic \
                       ON ic.object_id = c.object_id AND ic.column_id = c.column_id \
                     LEFT JOIN {db}.sys.default_constraints dc ON dc.object_id = c.default_object_id \
                     WHERE c.object_id = OBJECT_ID(@P1) ORDER BY c.column_id",
                    ty = type_expr("c"),
                ),
                &[&name],
            )
            .await?;
        if rows.is_empty() {
            return Err(DbError::query(format!("no table {name}")));
        }
        let mut lines: Vec<String> = rows
            .iter()
            .map(|r| {
                let mut line = format!("    {} {}", quote(&text(r, 0)), text(r, 1));
                if flag(r, 3) {
                    let seed = opt_text(r, 4).unwrap_or_else(|| "1".into());
                    let step = opt_text(r, 5).unwrap_or_else(|| "1".into());
                    line += &format!(" IDENTITY({seed}, {step})");
                }
                line += if flag(r, 2) { " NULL" } else { " NOT NULL" };
                if let Some(default) = opt_text(r, 6) {
                    line += &format!(" DEFAULT {default}");
                }
                line
            })
            .collect();
        let pk = self.indexes(table).await?.into_iter().find(|i| i.primary);
        if let Some(pk) = pk {
            let cols: Vec<String> = pk.columns.iter().map(|c| quote(c)).collect();
            lines.push(format!(
                "    CONSTRAINT {} PRIMARY KEY ({})",
                quote(&pk.name),
                cols.join(", ")
            ));
        }
        let schema = table.schema.as_deref().unwrap_or("dbo");
        Ok(format!(
            "CREATE TABLE {}.{} (\n{}\n);",
            quote(schema),
            quote(&table.name),
            lines.join(",\n")
        ))
    }
}

#[async_trait]
impl Connection for SqlServerConnection {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            schemas: true,
            cross_database: true,
            explain: true,
            explain_analyze: true,
        }
    }

    async fn execute(&mut self, sql: &str, sink: &mut ResultSink) -> Result<()> {
        let token = self.cancel.lock().unwrap().clone();
        let client = self.client()?;
        for batch in split(sql) {
            run(client, &batch, sink, &token).await?;
        }
        Ok(())
    }

    async fn explain(&mut self, sql: &str, analyze: bool) -> Result<Plan> {
        let token = self.cancel.lock().unwrap().clone();
        let batches = split(sql);
        let statement = batches
            .iter()
            .find(|s| !s.text.trim().is_empty())
            .ok_or_else(|| DbError::Other("Nothing to explain.".into()))?;
        // ANALYZE runs the statement; undo it, keeping the user's own
        // transaction if there is one
        let (open, close) = match analyze {
            false => (None, None),
            true if self.in_transaction().await? => (
                Some("SAVE TRANSACTION honcho_explain"),
                Some("ROLLBACK TRANSACTION honcho_explain"),
            ),
            true => (Some("BEGIN TRANSACTION"), Some("ROLLBACK TRANSACTION")),
        };
        let setting = if analyze {
            "STATISTICS XML"
        } else {
            "SHOWPLAN_XML"
        };
        let client = self.client()?;
        // each SET has to be a batch of its own
        exec(client, &format!("SET {setting} ON")).await?;
        let result = async {
            if let Some(open) = open {
                exec(client, open).await?;
            }
            let plans = plan_xml(client, statement, &token).await;
            if let Some(close) = close {
                let undone = exec(client, close).await;
                if plans.is_ok() {
                    undone?;
                }
            }
            plans
        }
        .await;
        // left on, every later query would answer with a plan instead
        let off = exec(client, &format!("SET {setting} OFF")).await;
        let plans = result?;
        off?;
        explain::parse_all(&plans, analyze)
    }

    fn statements<'a>(&self, sql: &'a str) -> Vec<Statement<'a>> {
        split(sql).iter().flat_map(split::statements).collect()
    }

    fn batches<'a>(&self, sql: &'a str) -> Vec<Statement<'a>> {
        split(sql)
    }

    fn canceller(&self) -> Option<Box<dyn Canceller>> {
        self.client.as_ref()?;
        let token = CancellationToken::new();
        *self.cancel.lock().unwrap() = token.clone();
        Some(Box::new(SqlServerCanceller(token)))
    }

    async fn ping(&mut self) -> Result<()> {
        self.query("SELECT 1", &[]).await.map(drop)
    }

    async fn current_database(&mut self) -> Result<Option<String>> {
        let rows = self.query("SELECT DB_NAME()", &[]).await?;
        Ok(rows.first().and_then(|r| opt_text(r, 0)))
    }

    async fn in_transaction(&mut self) -> Result<bool> {
        let rows = self.query("SELECT @@TRANCOUNT", &[]).await?;
        Ok(rows
            .first()
            .and_then(|r| r.get::<i32, _>(0))
            .is_some_and(|n| n > 0))
    }

    async fn use_database(&mut self, name: &str) -> Result<()> {
        self.client()?
            .simple_query(format!("USE {}", quote(name)))
            .await
            .map_err(query_error)?
            .into_results()
            .await
            .map_err(query_error)
            .map(drop)
    }

    async fn databases(&mut self) -> Result<Vec<String>> {
        let rows = self
            .query(
                "SELECT name FROM sys.databases WHERE HAS_DBACCESS(name) = 1 ORDER BY name",
                &[],
            )
            .await?;
        Ok(rows.iter().map(|r| text(r, 0)).collect())
    }

    async fn schemas(&mut self, database: &str) -> Result<Vec<String>> {
        // ids from 16384 are the fixed database roles' schemas
        let db = quote(database);
        let rows = self
            .query(
                &format!(
                    "SELECT s.name FROM {db}.sys.schemas s \
                     WHERE s.schema_id < 16384 AND s.name NOT IN ('sys', 'INFORMATION_SCHEMA') \
                       AND (s.name <> 'guest' OR EXISTS \
                         (SELECT 1 FROM {db}.sys.objects o WHERE o.schema_id = s.schema_id)) \
                     ORDER BY s.name"
                ),
                &[],
            )
            .await?;
        Ok(rows.iter().map(|r| text(r, 0)).collect())
    }

    async fn objects(&mut self, scope: &Scope, kinds: &[ObjectKind]) -> Result<Vec<ObjectRef>> {
        let db = self.database_or_current(&scope.database).await?;
        let q = quote(&db);
        let schema_filter = if scope.schema.is_some() {
            "AND s.name = @P1"
        } else {
            ""
        };
        let params = format!(
            "(SELECT p.name + ' ' + {ty} + CASE WHEN p.is_output = 1 THEN ' OUTPUT' ELSE '' END + ', ' \
              FROM {q}.sys.parameters p JOIN {q}.sys.types t ON t.user_type_id = p.user_type_id \
              WHERE p.object_id = o.object_id AND p.parameter_id > 0 ORDER BY p.parameter_id \
              FOR XML PATH(''), TYPE).value('.', 'nvarchar(max)')",
            ty = type_expr("p"),
        );
        let returns = format!(
            "(SELECT {ty} FROM {q}.sys.parameters p JOIN {q}.sys.types t ON t.user_type_id = p.user_type_id \
              WHERE p.object_id = o.object_id AND p.parameter_id = 0)",
            ty = type_expr("p"),
        );
        let sql = format!(
            "SELECT s.name, o.name, RTRIM(o.type), {params}, {returns} \
             FROM {q}.sys.objects o JOIN {q}.sys.schemas s ON s.schema_id = o.schema_id \
             WHERE o.is_ms_shipped = 0 AND o.type IN ({OBJECT_TYPES}) {schema_filter} \
             ORDER BY s.name, o.name"
        );
        let rows = match &scope.schema {
            Some(schema) => self.query(&sql, &[schema]).await?,
            None => self.query(&sql, &[]).await?,
        };
        let want = |k: ObjectKind| kinds.is_empty() || kinds.contains(&k);
        Ok(rows
            .iter()
            .filter_map(|r| {
                let kind = object_kind(&text(r, 2))?;
                if !want(kind) {
                    return None;
                }
                let signature = kind.is_routine().then(|| {
                    let params = opt_text(r, 3).unwrap_or_default();
                    let params = params.trim_end_matches(", ");
                    match (kind, opt_text(r, 4)) {
                        (ObjectKind::Function, Some(ret)) => format!("({params}) → {ret}"),
                        (ObjectKind::Function, None) => format!("({params}) → table"),
                        _ => format!("({params})"),
                    }
                });
                Some(ObjectRef {
                    signature,
                    ..ObjectRef::new(kind, text(r, 1))
                        .in_database(db.clone())
                        .in_schema(text(r, 0))
                })
            })
            .collect())
    }

    async fn columns(&mut self, object: &ObjectRef) -> Result<Vec<ColumnInfo>> {
        let name = self.full_name(object).await?;
        let db = quote(&self.database_or_current(&object.database).await?);
        let rows = self
            .query(
                &format!(
                    "SELECT c.name, {ty}, c.is_nullable, dc.definition, \
                     CAST(CASE WHEN EXISTS (SELECT 1 FROM {db}.sys.indexes i \
                       JOIN {db}.sys.index_columns ic \
                         ON ic.object_id = i.object_id AND ic.index_id = i.index_id \
                       WHERE i.object_id = c.object_id AND i.is_primary_key = 1 \
                         AND ic.column_id = c.column_id) THEN 1 ELSE 0 END AS bit), \
                     CAST(ep.value AS nvarchar(max)) \
                     FROM {db}.sys.columns c \
                     JOIN {db}.sys.types t ON t.user_type_id = c.user_type_id \
                     LEFT JOIN {db}.sys.default_constraints dc ON dc.object_id = c.default_object_id \
                     LEFT JOIN {db}.sys.extended_properties ep ON ep.class = 1 \
                       AND ep.major_id = c.object_id AND ep.minor_id = c.column_id \
                       AND ep.name = 'MS_Description' \
                     WHERE c.object_id = OBJECT_ID(@P1) ORDER BY c.column_id",
                    ty = type_expr("c"),
                ),
                &[&name],
            )
            .await?;
        if rows.is_empty() {
            return Err(DbError::query(format!("no table or view {name}")));
        }
        Ok(rows
            .iter()
            .map(|r| ColumnInfo {
                name: text(r, 0),
                data_type: text(r, 1),
                nullable: flag(r, 2),
                default: opt_text(r, 3),
                primary_key: flag(r, 4),
                comment: opt_text(r, 5).filter(|c| !c.is_empty()),
            })
            .collect())
    }

    async fn indexes(&mut self, table: &ObjectRef) -> Result<Vec<IndexInfo>> {
        let name = self.full_name(table).await?;
        let db = quote(&self.database_or_current(&table.database).await?);
        let rows = self
            .query(
                &format!(
                    "SELECT i.name, i.is_unique, i.is_primary_key, c.name \
                     FROM {db}.sys.indexes i \
                     JOIN {db}.sys.index_columns ic \
                       ON ic.object_id = i.object_id AND ic.index_id = i.index_id \
                     JOIN {db}.sys.columns c \
                       ON c.object_id = ic.object_id AND c.column_id = ic.column_id \
                     WHERE i.object_id = OBJECT_ID(@P1) AND i.name IS NOT NULL \
                       AND ic.is_included_column = 0 \
                     ORDER BY i.is_primary_key DESC, i.name, ic.key_ordinal"
                ),
                &[&name],
            )
            .await?;
        let mut out: Vec<IndexInfo> = Vec::new();
        for r in &rows {
            let index = text(r, 0);
            let column = text(r, 3);
            match out.last_mut() {
                Some(last) if last.name == index => last.columns.push(column),
                _ => out.push(IndexInfo {
                    name: index,
                    columns: vec![column],
                    unique: flag(r, 1),
                    primary: flag(r, 2),
                }),
            }
        }
        Ok(out)
    }

    async fn search(&mut self, term: &str, limit: usize) -> Result<Vec<ObjectRef>> {
        let sql = |db: &str| {
            let q = quote(db);
            format!(
                "SELECT TOP (@P2) s.name, o.name, RTRIM(o.type) \
                 FROM {q}.sys.objects o JOIN {q}.sys.schemas s ON s.schema_id = o.schema_id \
                 WHERE o.is_ms_shipped = 0 AND o.type IN ({OBJECT_TYPES}) \
                   AND LOWER(o.name) LIKE LOWER(@P1) ESCAPE '\\' \
                 ORDER BY s.name, o.name"
            )
        };
        self.search_each_database(term, limit, sql).await
    }

    async fn search_bodies(&mut self, term: &str, limit: usize) -> Result<Vec<ObjectRef>> {
        let sql = |db: &str| {
            let q = quote(db);
            format!(
                "SELECT TOP (@P2) s.name, o.name, RTRIM(o.type) \
                 FROM {q}.sys.sql_modules m \
                 JOIN {q}.sys.objects o ON o.object_id = m.object_id \
                 JOIN {q}.sys.schemas s ON s.schema_id = o.schema_id \
                 WHERE o.type IN ({ROUTINE_TYPES}) \
                   AND LOWER(m.definition) LIKE LOWER(@P1) ESCAPE '\\' \
                 ORDER BY s.name, o.name"
            )
        };
        self.search_each_database(term, limit, sql).await
    }

    async fn parameters(&mut self, routine: &ObjectRef) -> Result<Vec<RoutineParam>> {
        if !routine.kind.is_routine() {
            return Err(DbError::Unsupported("parameters of this object"));
        }
        let name = self.full_name(routine).await?;
        let db = quote(&self.database_or_current(&routine.database).await?);
        let rows = self
            .query(
                &format!(
                    "SELECT p.name, p.is_output, {ty} \
                     FROM {db}.sys.parameters p \
                     JOIN {db}.sys.types t ON t.user_type_id = p.user_type_id \
                     WHERE p.object_id = OBJECT_ID(@P1) AND p.parameter_id > 0 \
                     ORDER BY p.parameter_id",
                    ty = type_expr("p"),
                ),
                &[&name],
            )
            .await?;
        Ok(rows
            .iter()
            .map(|r| RoutineParam {
                name: text(r, 0),
                // T-SQL OUTPUT parameters also take a value in
                mode: if flag(r, 1) {
                    ParamMode::InOut
                } else {
                    ParamMode::In
                },
                data_type: text(r, 2),
            })
            .collect())
    }

    async fn definition(&mut self, object: &ObjectRef) -> Result<String> {
        match object.kind {
            ObjectKind::Table => return self.table_definition(object).await,
            ObjectKind::View
            | ObjectKind::Procedure
            | ObjectKind::Function
            | ObjectKind::Trigger => {}
            _ => return Err(DbError::Unsupported("definition for this object")),
        }
        let name = self.full_name(object).await?;
        let db = quote(&self.database_or_current(&object.database).await?);
        // OBJECT_DEFINITION only looks in the current database
        let rows = self
            .query(
                &format!(
                    "SELECT m.definition FROM {db}.sys.sql_modules m \
                     WHERE m.object_id = OBJECT_ID(@P1)"
                ),
                &[&name],
            )
            .await?;
        rows.first().and_then(|r| opt_text(r, 0)).ok_or_else(|| {
            DbError::query(
                "the server hides this definition from you; it needs the VIEW DEFINITION \
                 permission",
            )
        })
    }

    async fn close(&mut self) -> Result<()> {
        match self.client.take() {
            Some(client) => client.close().await.map_err(query_error),
            None => Ok(()),
        }
    }
}

impl SqlServerConnection {
    /// Runs `sql(database)` in every database until `limit` rows, skipping
    /// databases that are offline or closed to this login.
    async fn search_each_database(
        &mut self,
        term: &str,
        limit: usize,
        sql: impl Fn(&str) -> String,
    ) -> Result<Vec<ObjectRef>> {
        let pattern = format!("%{}%", like_escape(term));
        let mut out = Vec::new();
        for db in self.databases().await? {
            let left = limit.saturating_sub(out.len());
            if left == 0 {
                break;
            }
            let rows = match self.query(&sql(&db), &[&pattern, &(left as i64)]).await {
                Ok(rows) => rows,
                Err(e) if e.is_fatal() => return Err(e),
                Err(_) => continue,
            };
            out.extend(rows.iter().filter_map(|r| {
                let kind = object_kind(&text(r, 2))?;
                Some(
                    ObjectRef::new(kind, text(r, 1))
                        .in_database(db.clone())
                        .in_schema(text(r, 0)),
                )
            }));
        }
        Ok(out)
    }
}

enum Stop {
    Cancelled,
    Sink(DbError),
}

/// Runs one batch. A failed statement doesn't end the batch on the server,
/// so everything after it is still reported; the first error is returned
/// at the end and later ones become messages.
async fn run(
    client: &mut Client,
    batch: &Statement<'_>,
    sink: &mut ResultSink,
    token: &CancellationToken,
) -> Result<()> {
    let mut first_error = None;
    let stopped = {
        let mut stream = client.simple_batch(batch.text).await.map_err(query_error)?;
        let mut types: Vec<ColumnType> = Vec::new();
        let mut in_result = false;
        loop {
            let item = tokio::select! {
                biased;
                _ = token.cancelled() => break Some(Stop::Cancelled),
                item = stream.next() => item,
            };
            let item = match item {
                None => break None,
                Some(Ok(item)) => item,
                Some(Err(e)) => return Err(query_error(e)),
            };
            let sent = match item {
                BatchItem::Metadata(meta) => {
                    in_result = true;
                    types = meta.columns().iter().map(|c| c.column_type()).collect();
                    sink.columns(convert::columns(meta.columns())).await
                }
                BatchItem::Row(row) => {
                    let values = row
                        .into_iter()
                        .zip(&types)
                        .map(|(data, ty)| convert::value(*ty, data))
                        .collect();
                    sink.row(values).await
                }
                BatchItem::Done { rows } => {
                    if std::mem::take(&mut in_result) {
                        sink.done(None).await
                    } else if rows.is_some() {
                        sink.done(rows).await
                    } else {
                        // PRINT, DDL, or anything under SET NOCOUNT ON
                        Ok(())
                    }
                }
                BatchItem::Info(info) if CONTEXT_NOTICES.contains(&info.number()) => Ok(()),
                BatchItem::Info(info) => {
                    sink.message(Message {
                        level: if info.message().starts_with("Warning:") {
                            MessageLevel::Warning
                        } else {
                            MessageLevel::Info
                        },
                        text: info.message().to_string(),
                        // PRINT comes through as message 0
                        code: Some(info.number())
                            .filter(|&n| n != 0)
                            .map(|n| n.to_string()),
                    })
                    .await
                }
                BatchItem::Error(e) => {
                    let error = server_error(&e, batch.line);
                    match first_error {
                        None => {
                            first_error = Some(error);
                            Ok(())
                        }
                        Some(_) => {
                            sink.message(Message {
                                level: MessageLevel::Error,
                                text: error.to_string(),
                                code: Some(e.code().to_string()),
                            })
                            .await
                        }
                    }
                }
            };
            if let Err(e) = sent {
                break Some(Stop::Sink(e));
            }
        }
    };
    match stopped {
        Some(stop) => {
            // tells the server to stop, then reads up to its acknowledgement
            client.cancel_query().await.map_err(query_error)?;
            Err(match stop {
                Stop::Cancelled => DbError::Cancelled,
                Stop::Sink(e) => e,
            })
        }
        None if token.is_cancelled() => Err(DbError::Cancelled),
        None => first_error.map_or(Ok(()), Err),
    }
}

async fn exec(client: &mut Client, sql: &str) -> Result<()> {
    client
        .simple_query(sql)
        .await
        .map_err(query_error)?
        .into_results()
        .await
        .map_err(query_error)
        .map(drop)
}

/// Runs the statement and keeps only the plans among its results; with
/// STATISTICS XML its own rows come first and are read and dropped.
async fn plan_xml(
    client: &mut Client,
    statement: &Statement<'_>,
    token: &CancellationToken,
) -> Result<Vec<String>> {
    let mut plans = Vec::new();
    let mut first_error = None;
    let cancelled = {
        let mut stream = client
            .simple_batch(statement.text)
            .await
            .map_err(query_error)?;
        let mut is_plan = false;
        loop {
            let item = tokio::select! {
                biased;
                _ = token.cancelled() => break true,
                item = stream.next() => item,
            };
            match item {
                None => break false,
                Some(Err(e)) => return Err(query_error(e)),
                Some(Ok(BatchItem::Metadata(meta))) => {
                    is_plan = meta
                        .columns()
                        .first()
                        .is_some_and(|c| c.name() == explain::PLAN_COLUMN);
                }
                Some(Ok(BatchItem::Row(row))) if is_plan => match row.into_iter().next() {
                    Some(tiberius::ColumnData::Xml(Some(x))) => plans.push(x.to_string()),
                    Some(tiberius::ColumnData::String(Some(s))) => plans.push(s.to_string()),
                    _ => {}
                },
                Some(Ok(BatchItem::Error(e))) => {
                    first_error.get_or_insert_with(|| server_error(&e, statement.line));
                }
                Some(Ok(_)) => {}
            }
        }
    };
    if cancelled {
        client.cancel_query().await.map_err(query_error)?;
        return Err(DbError::Cancelled);
    }
    match first_error {
        Some(e) => Err(e),
        None => Ok(plans),
    }
}

struct SqlServerCanceller(CancellationToken);

#[async_trait]
impl Canceller for SqlServerCanceller {
    async fn cancel(&self) -> Result<()> {
        self.0.cancel();
        Ok(())
    }
}

fn object_kind(sys_type: &str) -> Option<ObjectKind> {
    Some(match sys_type {
        "U" => ObjectKind::Table,
        "V" => ObjectKind::View,
        "P" | "PC" => ObjectKind::Procedure,
        "FN" | "IF" | "TF" | "FS" | "FT" => ObjectKind::Function,
        "TR" => ObjectKind::Trigger,
        "SO" => ObjectKind::Sequence,
        _ => return None,
    })
}

/// T-SQL for the declared type of a column or parameter `x`, joined to
/// sys.types as `t`: `nvarchar(50)`, `decimal(12, 2)`, `varbinary(max)`.
fn type_expr(x: &str) -> String {
    format!(
        "(t.name + CASE \
           WHEN t.name IN ('varchar', 'char', 'varbinary', 'binary') THEN '(' + CASE WHEN {x}.max_length = -1 \
             THEN 'max' ELSE CAST({x}.max_length AS varchar(10)) END + ')' \
           WHEN t.name IN ('nvarchar', 'nchar') THEN '(' + CASE WHEN {x}.max_length = -1 \
             THEN 'max' ELSE CAST({x}.max_length / 2 AS varchar(10)) END + ')' \
           WHEN t.name IN ('decimal', 'numeric') THEN '(' + CAST({x}.precision AS varchar(10)) + ', ' \
             + CAST({x}.scale AS varchar(10)) + ')' \
           WHEN t.name IN ('datetime2', 'time', 'datetimeoffset') THEN '(' \
             + CAST({x}.scale AS varchar(10)) + ')' \
           ELSE '' END)"
    )
}

fn text(row: &Row, i: usize) -> String {
    opt_text(row, i).unwrap_or_default()
}

fn opt_text(row: &Row, i: usize) -> Option<String> {
    row.try_get::<&str, _>(i).ok().flatten().map(str::to_string)
}

fn flag(row: &Row, i: usize) -> bool {
    row.try_get::<bool, _>(i).ok().flatten().unwrap_or(false)
}

fn quote(ident: &str) -> String {
    format!("[{}]", ident.replace(']', "]]"))
}

/// `term` matched literally inside LIKE ... ESCAPE '\'.
fn like_escape(term: &str) -> String {
    let mut out = String::with_capacity(term.len());
    for c in term.chars() {
        if matches!(c, '%' | '_' | '[' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// An error from inside the batch, with its line moved from the batch to
/// the whole script.
fn server_error(e: &tiberius::error::TokenError, batch_line: u32) -> DbError {
    // errors raised inside a procedure count lines in the procedure
    let line = if e.procedure().is_empty() && e.line() > 0 {
        batch_line + e.line() - 1
    } else {
        batch_line
    };
    let message = match e.procedure() {
        "" => e.message().to_string(),
        procedure => format!("{procedure}: {}", e.message()),
    };
    DbError::Query {
        message,
        code: Some(e.code().to_string()),
        line: Some(line),
    }
}

/// The I/O errors behind a dropped connection read like library internals.
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
        io.to_string()
    }
}

fn connect_error(e: tiberius::error::Error) -> DbError {
    use tiberius::error::Error;
    match e {
        Error::Server(s) if AUTH_ERRORS.contains(&s.code()) => {
            DbError::Auth(s.message().to_string())
        }
        Error::Server(s) => DbError::Connect(s.message().to_string()),
        Error::Tls(e) => DbError::Tls(e),
        Error::Io { message, .. } if message.contains("certificate") => DbError::Tls(message),
        Error::Io { message, .. } => DbError::Connect(message),
        other => DbError::Connect(other.to_string()),
    }
}

fn query_error(e: tiberius::error::Error) -> DbError {
    use tiberius::error::Error;
    match e {
        Error::Server(s) => DbError::Query {
            message: s.message().to_string(),
            code: Some(s.code().to_string()),
            line: None,
        },
        Error::Io { message, .. } => DbError::ConnectionLost(lost_reason(&message)),
        other => DbError::Other(other.to_string()),
    }
}

#[cfg(test)]
mod tests;
