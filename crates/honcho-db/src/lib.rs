//! Database access for sql-honcho.
//!
//! Drivers implement [`Driver`] and [`Connection`]. The UI never calls them
//! directly: it goes through [`DbRuntime`] and [`Session`], which run drivers
//! on a background tokio runtime and hand results back over channels that can
//! be awaited from any executor.

mod driver;
mod error;
#[cfg(any(test, feature = "fake"))]
pub mod fake;
mod runtime;
mod sink;
mod types;
mod value;

pub use driver::{Canceller, Connection, Driver};
pub use error::DbError;
pub use runtime::{DbRuntime, QueryHandle, Session, SessionOptions};
pub use sink::ResultSink;
pub use types::*;
pub use value::Value;

pub type Result<T, E = DbError> = std::result::Result<T, E>;
