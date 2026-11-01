use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use crate::{DbError, Value};

pub type Row = Vec<Value>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DriverKind {
    MySql,
    SqlServer,
    Postgres,
    Sqlite,
    Fake,
}

/// How much TLS to insist on, from none to a checked chain and host name.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum TlsMode {
    Disable,
    /// TLS when the server offers it, without checking its certificate.
    #[default]
    Prefer,
    /// TLS or fail, without checking the certificate.
    Require,
    /// The certificate must chain to a trusted CA.
    VerifyCa,
    /// As `VerifyCa`, and it must name the host.
    VerifyFull,
}

impl TlsMode {
    pub const ALL: [TlsMode; 5] = [
        TlsMode::Disable,
        TlsMode::Prefer,
        TlsMode::Require,
        TlsMode::VerifyCa,
        TlsMode::VerifyFull,
    ];
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct TlsConfig {
    pub mode: TlsMode,
    /// Trusted CA, on top of the built-in roots.
    pub ca_file: Option<PathBuf>,
    /// Client certificate and key, for servers that ask for one.
    pub cert_file: Option<PathBuf>,
    pub key_file: Option<PathBuf>,
    /// Name to check the certificate against instead of the host, for when
    /// the host is a tunnel's local end.
    #[serde(skip)]
    pub server_name: Option<String>,
}

/// What a driver needs to open a connection.
#[derive(Clone, Default, PartialEq)]
pub struct ConnectConfig {
    pub host: String,
    pub port: Option<u16>,
    pub user: String,
    pub password: Option<String>,
    pub database: Option<String>,
    pub connect_timeout: Option<Duration>,
    /// Refuse anything but reads, in the session and on the server where
    /// the driver can.
    pub read_only: bool,
    pub tls: TlsConfig,
    /// Reach the server through this SSH server instead of directly.
    pub ssh: Option<crate::tunnel::SshConfig>,
    /// Driver-specific options, passed through as-is.
    pub options: BTreeMap<String, String>,
}

impl fmt::Debug for ConnectConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConnectConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("password", &self.password.as_ref().map(|_| "***"))
            .field("database", &self.database)
            .field("connect_timeout", &self.connect_timeout)
            .field("read_only", &self.read_only)
            .field("tls", &self.tls)
            .field("ssh", &self.ssh)
            .field("options", &self.options)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Column {
    pub name: String,
    /// Type name as the server reports it, e.g. `varchar(255)` or `int4`.
    pub type_name: String,
    pub nullable: Option<bool>,
    /// Table the column was read from, when the server says.
    pub table: Option<String>,
}

impl Column {
    pub fn new(name: impl Into<String>, type_name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            type_name: type_name.into(),
            nullable: None,
            table: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageLevel {
    Info,
    Warning,
    Error,
}

/// Non-fatal server output: MySQL warnings, SQL Server PRINT, Postgres NOTICE.
#[derive(Clone, Debug, PartialEq)]
pub struct Message {
    pub level: MessageLevel,
    pub text: String,
    pub code: Option<String>,
}

impl Message {
    pub fn info(text: impl Into<String>) -> Self {
        Self {
            level: MessageLevel::Info,
            text: text.into(),
            code: None,
        }
    }

    pub fn warning(text: impl Into<String>) -> Self {
        Self {
            level: MessageLevel::Warning,
            text: text.into(),
            code: None,
        }
    }
}

/// What a running query reports, in order.
///
/// A result set is `Columns`, any number of `Rows`, then `Done`. A statement
/// without a result set is just `Done`. `Finished` is always last.
#[derive(Clone, Debug, PartialEq)]
pub enum QueryEvent {
    Columns(Vec<Column>),
    Rows(Vec<Row>),
    Done { rows_affected: Option<u64> },
    Message(Message),
    Finished(Result<QueryStats, DbError>),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct QueryStats {
    pub elapsed: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ObjectKind {
    Table,
    View,
    MaterializedView,
    Procedure,
    Function,
    Trigger,
    Sequence,
}

impl ObjectKind {
    pub fn is_routine(self) -> bool {
        matches!(self, ObjectKind::Procedure | ObjectKind::Function)
    }
}

/// Identifies a database object. `schema` is `None` on servers where the
/// database is the schema (MySQL).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ObjectRef {
    pub database: Option<String>,
    pub schema: Option<String>,
    pub name: String,
    pub kind: ObjectKind,
    /// Argument types, to tell overloaded functions apart (Postgres).
    pub signature: Option<String>,
}

impl ObjectRef {
    pub fn new(kind: ObjectKind, name: impl Into<String>) -> Self {
        Self {
            database: None,
            schema: None,
            name: name.into(),
            kind,
            signature: None,
        }
    }

    pub fn in_database(mut self, database: impl Into<String>) -> Self {
        self.database = Some(database.into());
        self
    }

    pub fn in_schema(mut self, schema: impl Into<String>) -> Self {
        self.schema = Some(schema.into());
        self
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ColumnInfo {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub default: Option<String>,
    pub primary_key: bool,
    pub comment: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexInfo {
    pub name: String,
    /// In index order.
    pub columns: Vec<String>,
    pub unique: bool,
    pub primary: bool,
}

/// Where to list objects. Unset fields mean the connection's current one.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Scope {
    pub database: Option<String>,
    pub schema: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capabilities {
    /// Databases contain schemas (SQL Server, Postgres). False when a
    /// database is a schema (MySQL) or there is only one (SQLite).
    pub schemas: bool,
    /// One connection can query across databases (MySQL, SQL Server).
    pub cross_database: bool,
}
