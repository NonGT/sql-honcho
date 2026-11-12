//! Database access for sql-honcho.
//!
//! Drivers implement [`Driver`] and [`Connection`]. The UI never calls them
//! directly: it goes through [`DbRuntime`] and [`Session`], which run drivers
//! on a background tokio runtime and hand results back over channels that can
//! be awaited from any executor.

pub mod conformance;
mod driver;
mod error;
#[cfg(any(test, feature = "fake"))]
pub mod fake;
pub mod mysql;
pub mod plan;
pub mod postgres;
pub mod project;
pub mod routine;
mod runtime;
pub mod script;
mod sink;
pub mod sqlite;
pub mod sqlserver;
pub mod tunnel;
mod types;
mod value;

pub use driver::{Canceller, Connection, Driver};
pub use error::DbError;
pub use plan::{Plan, PlanFormat, PlanNode};
pub use runtime::{DbRuntime, QueryHandle, RunMode, Session, SessionOptions};
pub use sink::ResultSink;
pub use types::*;
pub use value::Value;

pub type Result<T, E = DbError> = std::result::Result<T, E>;
