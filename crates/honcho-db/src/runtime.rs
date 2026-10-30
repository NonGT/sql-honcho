use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::runtime::{Handle, Runtime};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::types::{
    Capabilities, ColumnInfo, ConnectConfig, ObjectKind, ObjectRef, QueryEvent, QueryStats, Scope,
};
use crate::{Connection, DbError, Driver, Result, ResultSink};

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
type Job =
    Box<dyn for<'a> FnOnce(Option<&'a mut (dyn Connection + 'static)>) -> BoxFuture<'a, ()> + Send>;

const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Background tokio runtime that owns every connection.
///
/// Everything it hands back (sessions, query handles, the futures from
/// metadata calls) can be awaited from any executor, including gpui's.
pub struct DbRuntime {
    rt: Option<Runtime>,
}

impl DbRuntime {
    pub fn new() -> std::io::Result<Self> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("honcho-db")
            .enable_all()
            .build()?;
        Ok(Self { rt: Some(rt) })
    }

    fn handle(&self) -> &Handle {
        self.rt
            .as_ref()
            .expect("runtime is alive until drop")
            .handle()
    }

    pub fn connect(
        &self,
        driver: Arc<dyn Driver>,
        config: ConnectConfig,
        options: SessionOptions,
    ) -> impl Future<Output = Result<Session>> + Send + 'static {
        let (tx, rx) = oneshot::channel();
        let handle = self.handle().clone();
        self.handle().spawn(async move {
            let timeout = config.connect_timeout.unwrap_or(DEFAULT_CONNECT_TIMEOUT);
            let result = match tokio::time::timeout(timeout, driver.connect(&config)).await {
                Ok(Ok(conn)) => Ok(Session::spawn(&handle, conn, options)),
                Ok(Err(e)) => Err(e),
                Err(_) => Err(DbError::Connect(format!(
                    "timed out after {}s",
                    timeout.as_secs()
                ))),
            };
            let _ = tx.send(result);
        });
        async move { rx.await.unwrap_or_else(|_| Err(stopped())) }
    }
}

impl Drop for DbRuntime {
    fn drop(&mut self) {
        // a plain drop panics when it happens inside another async runtime
        if let Some(rt) = self.rt.take() {
            rt.shutdown_background();
        }
    }
}

fn stopped() -> DbError {
    DbError::Other("database runtime stopped".into())
}

fn closed() -> DbError {
    DbError::ConnectionLost("session closed".into())
}

#[derive(Clone, Copy, Debug)]
pub struct SessionOptions {
    /// How long a cancelled query gets to stop before the connection is
    /// given up as unusable.
    pub cancel_grace: Duration,
}

impl Default for SessionOptions {
    fn default() -> Self {
        Self {
            cancel_grace: Duration::from_secs(5),
        }
    }
}

enum Command {
    Execute {
        sql: String,
        events: mpsc::Sender<QueryEvent>,
        cancel: CancellationToken,
    },
    Call(Job),
    Close(oneshot::Sender<()>),
}

/// A live connection. Clones share it; work is queued and runs in order.
#[derive(Clone)]
pub struct Session {
    tx: mpsc::UnboundedSender<Command>,
    capabilities: Capabilities,
}

impl Session {
    fn spawn(handle: &Handle, conn: Box<dyn Connection>, options: SessionOptions) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let capabilities = conn.capabilities();
        handle.spawn(run_session(conn, rx, options));
        Self { tx, capabilities }
    }

    pub fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    /// Queue SQL to run. Events arrive on the handle, ending with `Finished`.
    pub fn execute(&self, sql: impl Into<String>) -> QueryHandle {
        let (events_tx, events) = mpsc::channel(64);
        let cancel = CancellationToken::new();
        let command = Command::Execute {
            sql: sql.into(),
            events: events_tx,
            cancel: cancel.clone(),
        };
        if let Err(mpsc::error::SendError(Command::Execute { events, .. })) = self.tx.send(command)
        {
            let _ = events.try_send(QueryEvent::Finished(Err(closed())));
        }
        QueryHandle {
            events,
            cancel,
            finished: false,
        }
    }

    async fn call<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: for<'a> FnOnce(&'a mut (dyn Connection + 'static)) -> BoxFuture<'a, Result<T>>
            + Send
            + 'static,
    {
        let (tx, rx) = oneshot::channel();
        let job: Job = Box::new(move |conn| {
            Box::pin(async move {
                let result = match conn {
                    Some(conn) => f(conn).await,
                    None => Err(DbError::ConnectionLost(
                        "connection is broken, reconnect".into(),
                    )),
                };
                let _ = tx.send(result);
            })
        });
        self.tx.send(Command::Call(job)).map_err(|_| closed())?;
        rx.await.map_err(|_| closed())?
    }

    pub async fn ping(&self) -> Result<()> {
        self.call(|c| c.ping()).await
    }

    pub async fn current_database(&self) -> Result<Option<String>> {
        self.call(|c| c.current_database()).await
    }

    pub async fn use_database(&self, name: impl Into<String>) -> Result<()> {
        let name = name.into();
        self.call(move |c| Box::pin(async move { c.use_database(&name).await }))
            .await
    }

    pub async fn databases(&self) -> Result<Vec<String>> {
        self.call(|c| c.databases()).await
    }

    pub async fn schemas(&self, database: impl Into<String>) -> Result<Vec<String>> {
        let database = database.into();
        self.call(move |c| Box::pin(async move { c.schemas(&database).await }))
            .await
    }

    pub async fn objects(&self, scope: Scope, kinds: Vec<ObjectKind>) -> Result<Vec<ObjectRef>> {
        self.call(move |c| Box::pin(async move { c.objects(&scope, &kinds).await }))
            .await
    }

    pub async fn columns(&self, object: ObjectRef) -> Result<Vec<ColumnInfo>> {
        self.call(move |c| Box::pin(async move { c.columns(&object).await }))
            .await
    }

    pub async fn definition(&self, object: ObjectRef) -> Result<String> {
        self.call(move |c| Box::pin(async move { c.definition(&object).await }))
            .await
    }

    /// Close after queued work finishes.
    pub async fn close(&self) {
        let (tx, rx) = oneshot::channel();
        if self.tx.send(Command::Close(tx)).is_ok() {
            let _ = rx.await;
        }
    }
}

enum Outcome {
    Done(Result<()>),
    Cancelled,
    /// The driver didn't stop within the grace period.
    Abandoned,
}

async fn run_session(
    mut conn: Box<dyn Connection>,
    mut rx: mpsc::UnboundedReceiver<Command>,
    options: SessionOptions,
) {
    let mut broken: Option<String> = None;

    while let Some(command) = rx.recv().await {
        match command {
            Command::Execute {
                sql,
                events,
                cancel,
            } => {
                let mut sink = ResultSink::new(events);
                if let Some(reason) = &broken {
                    sink.finish(Err(DbError::ConnectionLost(reason.clone())))
                        .await;
                    continue;
                }
                if cancel.is_cancelled() {
                    sink.finish(Err(DbError::Cancelled)).await;
                    continue;
                }

                let canceller = conn.canceller();
                let started = Instant::now();
                let outcome = {
                    let run = conn.execute(&sql, &mut sink);
                    tokio::pin!(run);
                    tokio::select! {
                        biased;
                        result = &mut run => Outcome::Done(result),
                        _ = cancel.cancelled() => {
                            if let Some(c) = &canceller {
                                let _ = c.cancel().await;
                            }
                            match tokio::time::timeout(options.cancel_grace, &mut run).await {
                                Ok(_) => Outcome::Cancelled,
                                Err(_) => Outcome::Abandoned,
                            }
                        }
                    }
                };

                let result = match outcome {
                    Outcome::Done(Ok(())) => Ok(QueryStats {
                        elapsed: started.elapsed(),
                    }),
                    Outcome::Done(Err(e)) => {
                        if e.is_fatal() {
                            broken = Some(e.to_string());
                        }
                        Err(e)
                    }
                    Outcome::Cancelled => Err(DbError::Cancelled),
                    Outcome::Abandoned => {
                        broken = Some("query didn't stop after cancel".into());
                        Err(DbError::Cancelled)
                    }
                };
                sink.finish(result).await;
            }
            Command::Call(job) => {
                let conn = if broken.is_some() {
                    None
                } else {
                    Some(&mut *conn)
                };
                job(conn).await;
            }
            Command::Close(done) => {
                shut_down(conn, rx).await;
                let _ = done.send(());
                return;
            }
        }
    }
    let _ = conn.close().await;
}

// Stop taking work first, then answer whatever was already queued, so a
// query sent around the same time as close still gets its Finished event.
async fn shut_down(mut conn: Box<dyn Connection>, mut rx: mpsc::UnboundedReceiver<Command>) {
    rx.close();
    while let Ok(command) = rx.try_recv() {
        match command {
            Command::Execute { events, .. } => {
                ResultSink::new(events).finish(Err(closed())).await;
            }
            Command::Call(job) => job(None).await,
            Command::Close(done) => {
                let _ = done.send(());
            }
        }
    }
    let _ = conn.close().await;
}

/// A running query. Dropping it before `Finished` cancels the query.
pub struct QueryHandle {
    events: mpsc::Receiver<QueryEvent>,
    cancel: CancellationToken,
    finished: bool,
}

impl QueryHandle {
    /// Next event, or `None` after `Finished`.
    pub async fn next(&mut self) -> Option<QueryEvent> {
        if self.finished {
            return None;
        }
        // the session promises a Finished; make that hold even if it vanished
        let event = self
            .events
            .recv()
            .await
            .unwrap_or(QueryEvent::Finished(Err(closed())));
        self.finished = matches!(event, QueryEvent::Finished(_));
        Some(event)
    }

    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// For cancelling from somewhere that doesn't own the handle.
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }
}

impl Drop for QueryHandle {
    fn drop(&mut self) {
        if !self.finished {
            self.cancel.cancel();
        }
    }
}

#[cfg(test)]
mod tests;
