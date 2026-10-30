use async_trait::async_trait;

use crate::types::{
    Capabilities, ColumnInfo, ConnectConfig, DriverKind, ObjectKind, ObjectRef, Scope,
};
use crate::{Result, ResultSink};

#[async_trait]
pub trait Driver: Send + Sync {
    fn kind(&self) -> DriverKind;

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

    /// Something that can interrupt the next `execute` from another task.
    /// Taken right before each run.
    fn canceller(&self) -> Option<Box<dyn Canceller>>;

    async fn ping(&mut self) -> Result<()>;

    async fn current_database(&mut self) -> Result<Option<String>>;

    async fn use_database(&mut self, name: &str) -> Result<()>;

    async fn databases(&mut self) -> Result<Vec<String>>;

    /// Schemas in a database. Empty when the server has no schema level.
    async fn schemas(&mut self, _database: &str) -> Result<Vec<String>> {
        Ok(Vec::new())
    }

    async fn objects(&mut self, scope: &Scope, kinds: &[ObjectKind]) -> Result<Vec<ObjectRef>>;

    async fn columns(&mut self, object: &ObjectRef) -> Result<Vec<ColumnInfo>>;

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
