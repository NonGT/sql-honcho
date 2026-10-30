use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use crate::types::{Column, Message, QueryEvent, QueryStats, Row};
use crate::{DbError, Result};

// Small first batch so the grid shows something right away.
const FIRST_BATCH: usize = 100;
const BATCH: usize = 2000;
const MAX_HOLD: Duration = Duration::from_millis(100);

/// Where a driver reports results. Rows are buffered and sent in batches.
pub struct ResultSink {
    tx: mpsc::Sender<QueryEvent>,
    rows: Vec<Row>,
    batch: usize,
    last_flush: Instant,
}

impl ResultSink {
    pub(crate) fn new(tx: mpsc::Sender<QueryEvent>) -> Self {
        Self {
            tx,
            rows: Vec::new(),
            batch: FIRST_BATCH,
            last_flush: Instant::now(),
        }
    }

    /// Start a new result set.
    pub async fn columns(&mut self, columns: Vec<Column>) -> Result<()> {
        self.flush().await?;
        self.batch = FIRST_BATCH;
        self.send(QueryEvent::Columns(columns)).await
    }

    pub async fn row(&mut self, row: Row) -> Result<()> {
        self.rows.push(row);
        if self.rows.len() >= self.batch || self.last_flush.elapsed() >= MAX_HOLD {
            self.flush().await?;
            self.batch = BATCH;
        }
        Ok(())
    }

    /// End of a statement, closing its result set if it had one.
    pub async fn done(&mut self, rows_affected: Option<u64>) -> Result<()> {
        self.flush().await?;
        self.send(QueryEvent::Done { rows_affected }).await
    }

    pub async fn message(&mut self, message: Message) -> Result<()> {
        self.flush().await?;
        self.send(QueryEvent::Message(message)).await
    }

    pub(crate) async fn finish(mut self, result: Result<QueryStats>) {
        let result = match self.flush().await {
            Ok(()) => result,
            Err(e) => Err(e),
        };
        let _ = self.tx.send(QueryEvent::Finished(result)).await;
    }

    async fn flush(&mut self) -> Result<()> {
        self.last_flush = Instant::now();
        if self.rows.is_empty() {
            return Ok(());
        }
        let rows = std::mem::take(&mut self.rows);
        self.send(QueryEvent::Rows(rows)).await
    }

    async fn send(&mut self, event: QueryEvent) -> Result<()> {
        // the only way this fails is the query handle being dropped
        self.tx.send(event).await.map_err(|_| DbError::Cancelled)
    }
}
