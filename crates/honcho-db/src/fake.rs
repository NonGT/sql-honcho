//! A scripted in-memory driver. Each `;`-separated statement is a command:
//!
//! - `rows N`: a result set of N rows (`id`, `name`)
//! - `trickle N MS`: N rows, one every MS milliseconds
//! - `types`: one row with a value of each kind (see [`types_row`])
//! - `update N`: a statement that affected N rows
//! - `warn TEXT`: a warning message
//! - `sleep MS`: wait, stopping early when cancelled
//! - `hang MS`: wait, ignoring cancel
//! - `error TEXT`: fail with a query error
//! - `lose`: fail as if the connection dropped
//!
//! Connecting as user `bad` fails authentication; user `slow` never finishes
//! connecting.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::types::*;
use crate::{Canceller, Connection, DbError, Driver, Result, ResultSink, Value};

#[derive(Default)]
pub struct FakeDriver {
    closed: Arc<AtomicUsize>,
}

impl FakeDriver {
    /// How many connections from this driver have been closed.
    pub fn closed_count(&self) -> usize {
        self.closed.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Driver for FakeDriver {
    fn kind(&self) -> DriverKind {
        DriverKind::Fake
    }

    async fn connect(&self, config: &ConnectConfig) -> Result<Box<dyn Connection>> {
        match config.user.as_str() {
            "bad" => {
                return Err(DbError::Auth(format!(
                    "access denied for user '{}'",
                    config.user
                )))
            }
            "slow" => std::future::pending::<()>().await,
            _ => {}
        }
        Ok(Box::new(FakeConnection {
            database: config.database.clone(),
            cancel: Arc::new(Mutex::new(CancellationToken::new())),
            closed: self.closed.clone(),
        }))
    }
}

pub struct FakeConnection {
    database: Option<String>,
    cancel: Arc<Mutex<CancellationToken>>,
    closed: Arc<AtomicUsize>,
}

struct FakeCanceller(CancellationToken);

#[async_trait]
impl Canceller for FakeCanceller {
    async fn cancel(&self) -> Result<()> {
        self.0.cancel();
        Ok(())
    }
}

const DATABASES: &[&str] = &["app", "analytics"];

type TypeCase = (&'static str, &'static str, fn() -> Value);

const TYPES_ROW: &[TypeCase] = &[
    ("n", "bigint", || Value::Int(42)),
    ("d", "decimal(12,2)", || Value::Decimal("12.50".into())),
    ("t", "varchar(64)", || Value::Text("สวัสดี 😀".into())),
    ("z", "varchar(1)", || Value::Null),
    ("day", "date", || Value::Date("2024-01-31".into())),
];

/// The row the `types` command returns.
pub fn types_row() -> Vec<Value> {
    TYPES_ROW.iter().map(|(_, _, v)| v()).collect()
}

fn objects_in(database: &str) -> Vec<ObjectRef> {
    let mk = |kind, name: &str| ObjectRef::new(kind, name).in_database(database);
    vec![
        mk(ObjectKind::Table, "orders"),
        mk(ObjectKind::Table, "users"),
        mk(ObjectKind::View, "active_users"),
        mk(ObjectKind::Procedure, "refresh_stats"),
        mk(ObjectKind::Function, "order_total"),
    ]
}

#[async_trait]
impl Connection for FakeConnection {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            schemas: false,
            cross_database: true,
        }
    }

    async fn execute(&mut self, sql: &str, sink: &mut ResultSink) -> Result<()> {
        let cancel = self.cancel.lock().unwrap().clone();
        for statement in sql.split(';').map(str::trim).filter(|s| !s.is_empty()) {
            let (cmd, arg) = statement.split_once(' ').unwrap_or((statement, ""));
            let n = || {
                arg.trim()
                    .parse::<u64>()
                    .map_err(|_| DbError::query(format!("bad number: {arg}")))
            };
            match cmd {
                "rows" => {
                    sink.columns(vec![
                        Column::new("id", "int"),
                        Column::new("name", "varchar(64)"),
                    ])
                    .await?;
                    for i in 0..n()? {
                        sink.row(vec![Value::Int(i as i64), Value::Text(format!("row {i}"))])
                            .await?;
                    }
                    sink.done(None).await?;
                }
                "trickle" => {
                    let (count, ms) = arg
                        .split_once(' ')
                        .and_then(|(a, b)| Some((a.parse::<u64>().ok()?, b.parse::<u64>().ok()?)))
                        .ok_or_else(|| DbError::query(format!("bad trickle args: {arg}")))?;
                    sink.columns(vec![Column::new("id", "int")]).await?;
                    for i in 0..count {
                        tokio::time::sleep(Duration::from_millis(ms)).await;
                        sink.row(vec![Value::Int(i as i64)]).await?;
                    }
                    sink.done(None).await?;
                }
                "types" => {
                    let cols = TYPES_ROW
                        .iter()
                        .map(|(n, t, _)| Column::new(*n, *t))
                        .collect();
                    sink.columns(cols).await?;
                    sink.row(types_row()).await?;
                    sink.done(None).await?;
                }
                "update" => sink.done(Some(n()?)).await?,
                "warn" => sink.message(Message::warning(arg)).await?,
                "sleep" => {
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_millis(n()?)) => {}
                        _ = cancel.cancelled() => return Err(DbError::Cancelled),
                    }
                }
                "hang" => tokio::time::sleep(Duration::from_millis(n()?)).await,
                "error" => return Err(DbError::query(arg)),
                "lose" => return Err(DbError::ConnectionLost("server went away".into())),
                other => return Err(DbError::query(format!("unknown fake command: {other}"))),
            }
        }
        Ok(())
    }

    fn canceller(&self) -> Option<Box<dyn Canceller>> {
        let token = CancellationToken::new();
        *self.cancel.lock().unwrap() = token.clone();
        Some(Box::new(FakeCanceller(token)))
    }

    async fn ping(&mut self) -> Result<()> {
        Ok(())
    }

    async fn current_database(&mut self) -> Result<Option<String>> {
        Ok(self.database.clone())
    }

    async fn use_database(&mut self, name: &str) -> Result<()> {
        if !DATABASES.contains(&name) {
            return Err(DbError::query(format!("unknown database '{name}'")));
        }
        self.database = Some(name.to_string());
        Ok(())
    }

    async fn databases(&mut self) -> Result<Vec<String>> {
        Ok(DATABASES.iter().map(|s| s.to_string()).collect())
    }

    async fn objects(&mut self, scope: &Scope, kinds: &[ObjectKind]) -> Result<Vec<ObjectRef>> {
        let db = scope
            .database
            .clone()
            .or_else(|| self.database.clone())
            .ok_or_else(|| DbError::query("no database selected"))?;
        Ok(objects_in(&db)
            .into_iter()
            .filter(|o| kinds.is_empty() || kinds.contains(&o.kind))
            .collect())
    }

    async fn columns(&mut self, object: &ObjectRef) -> Result<Vec<ColumnInfo>> {
        let col = |name: &str, ty: &str, pk: bool| ColumnInfo {
            name: name.into(),
            data_type: ty.into(),
            nullable: !pk,
            default: None,
            primary_key: pk,
            comment: None,
        };
        match object.name.as_str() {
            "users" => Ok(vec![
                col("id", "int", true),
                col("email", "varchar(255)", false),
            ]),
            "orders" => Ok(vec![
                col("id", "int", true),
                col("user_id", "int", false),
                col("amount", "decimal(12,2)", false),
            ]),
            other => Err(DbError::query(format!("no such table: {other}"))),
        }
    }

    async fn definition(&mut self, object: &ObjectRef) -> Result<String> {
        match (object.kind, object.name.as_str()) {
            (ObjectKind::Procedure, "refresh_stats") => {
                Ok("CREATE PROCEDURE refresh_stats()\nBEGIN\n  SELECT 1;\nEND".into())
            }
            (ObjectKind::Function, "order_total") => {
                Ok("CREATE FUNCTION order_total(p_id INT) RETURNS DECIMAL(12,2)\nRETURN 0".into())
            }
            _ => Err(DbError::Unsupported("definition for this object")),
        }
    }

    async fn close(&mut self) -> Result<()> {
        self.closed.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}
