use async_trait::async_trait;

use crate::script::Statement;
use crate::types::{
    Capabilities, ColumnInfo, ConnectConfig, DriverKind, IndexInfo, ObjectKind, ObjectRef,
    RoutineParam, Scope,
};
use crate::{Plan, Result, ResultSink};

#[async_trait]
pub trait Driver: Send + Sync {
    fn kind(&self) -> DriverKind;

    /// Port to use when the config has none, for tunnels to aim at.
    fn default_port(&self) -> Option<u16> {
        None
    }

    async fn connect(&self, config: &ConnectConfig) -> Result<Box<dyn Connection>>;
}

/// One open connection. Calls are never concurrent: the session runs them
/// one at a time, and cancellation goes through [`Canceller`] instead.
#[async_trait]
pub trait Connection: Send {
    fn capabilities(&self) -> Capabilities;

    /// Run the SQL (possibly several statements) and report everything
    /// through `sink`. If a sink call fails the receiver is gone: stop and
    /// return that error.
    async fn execute(&mut self, sql: &str, sink: &mut ResultSink) -> Result<()>;

    /// The statements of the SQL, for checking read-only sessions. Drivers
    /// that send scripts whole needn't override it.
    /// The plan for one statement. With `analyze` the statement runs, and
    /// a driver that can undo its changes does.
    async fn explain(&mut self, _sql: &str, _analyze: bool) -> Result<Plan> {
        Err(crate::DbError::Unsupported("explain"))
    }

    fn statements<'a>(&self, sql: &'a str) -> Vec<Statement<'a>> {
        vec![Statement::whole(sql)]
    }

    /// Something that can interrupt the next `execute` from another task.
    /// Taken right before each run.
    fn canceller(&self) -> Option<Box<dyn Canceller>>;

    async fn ping(&mut self) -> Result<()>;

    async fn current_database(&mut self) -> Result<Option<String>>;

    async fn use_database(&mut self, name: &str) -> Result<()>;

    /// Whether a transaction is open, which closing the connection would
    /// roll back. Drivers that can't tell say no.
    async fn in_transaction(&mut self) -> Result<bool> {
        Ok(false)
    }

    async fn databases(&mut self) -> Result<Vec<String>>;

    /// Schemas in a database. Empty when the server has no schema level.
    async fn schemas(&mut self, _database: &str) -> Result<Vec<String>> {
        Ok(Vec::new())
    }

    async fn objects(&mut self, scope: &Scope, kinds: &[ObjectKind]) -> Result<Vec<ObjectRef>>;

    async fn columns(&mut self, object: &ObjectRef) -> Result<Vec<ColumnInfo>>;

    /// Indexes of a table, the primary key first.
    async fn indexes(&mut self, _table: &ObjectRef) -> Result<Vec<IndexInfo>> {
        Err(crate::DbError::Unsupported("indexes"))
    }

    /// Objects in any database whose name contains `term`, case-insensitive,
    /// at most `limit` of them. For finding things in parts of the tree that
    /// haven't been loaded.
    async fn search(&mut self, _term: &str, _limit: usize) -> Result<Vec<ObjectRef>> {
        Err(crate::DbError::Unsupported("search"))
    }

    /// Routines whose code contains `term`, case-insensitive: which
    /// procedures touch a table, say.
    async fn search_bodies(&mut self, _term: &str, _limit: usize) -> Result<Vec<ObjectRef>> {
        Err(crate::DbError::Unsupported("searching routine bodies"))
    }

    /// A routine's parameters in order; a function's return value isn't one.
    async fn parameters(&mut self, _routine: &ObjectRef) -> Result<Vec<RoutineParam>> {
        Err(crate::DbError::Unsupported("routine parameters"))
    }

    /// Source of a view or routine, as a script that recreates it.
    async fn definition(&mut self, object: &ObjectRef) -> Result<String>;

    async fn close(&mut self) -> Result<()> {
        Ok(())
    }
}

#[async_trait]
pub trait Canceller: Send + Sync {
    async fn cancel(&self) -> Result<()>;
}
