#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum DbError {
    #[error("couldn't connect: {0}")]
    Connect(String),
    #[error("authentication failed: {0}")]
    Auth(String),
    #[error("TLS error: {0}")]
    Tls(String),
    #[error("SSH tunnel error: {0}")]
    Tunnel(String),
    #[error("{message}")]
    Query {
        message: String,
        /// Server error code, e.g. MySQL `1064` or a Postgres SQLSTATE.
        code: Option<String>,
        /// 1-based line in the submitted SQL, when the server reports one.
        line: Option<u32>,
    },
    #[error("query cancelled")]
    Cancelled,
    #[error("connection lost: {0}")]
    ConnectionLost(String),
    #[error("not supported by this driver: {0}")]
    Unsupported(&'static str),
    #[error("{0}")]
    Other(String),
}

impl DbError {
    pub fn query(message: impl Into<String>) -> Self {
        DbError::Query {
            message: message.into(),
            code: None,
            line: None,
        }
    }

    /// Errors after which the connection can't be trusted and should be reopened.
    pub fn is_fatal(&self) -> bool {
        matches!(self, DbError::ConnectionLost(_))
    }
}
