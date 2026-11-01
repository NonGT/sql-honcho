use std::sync::Arc;

use gpui::{App, Global, Hsla};
use honcho_config::{ColorTag, ConnectionStore, Keyring, SavedConnection, Secrets};
use honcho_db::{DbRuntime, Driver, DriverKind, Session};

use crate::theme::c;

/// Saved connections and what's needed to open them.
pub struct Connections {
    pub store: ConnectionStore,
    pub secrets: Arc<dyn Secrets>,
    pub runtime: Arc<DbRuntime>,
    /// Why the saved file couldn't be read. Saving is refused while set,
    /// so the file isn't replaced by an empty list.
    pub load_error: Option<String>,
}

impl Global for Connections {}

impl Connections {
    pub fn init(cx: &mut App) {
        let path = ConnectionStore::default_path()
            .unwrap_or_else(|| std::path::PathBuf::from("connections.json"));
        let (store, load_error) = match ConnectionStore::load(&path) {
            Ok(store) => (store, None),
            Err(e) => (ConnectionStore::empty(&path), Some(e.to_string())),
        };
        let runtime = DbRuntime::new().expect("couldn't start the database runtime");
        cx.set_global(Connections {
            store,
            secrets: Arc::new(Keyring),
            runtime: Arc::new(runtime),
            load_error,
        });
    }
}

/// A tab's open connection.
#[derive(Clone)]
pub struct ActiveConnection {
    pub saved: SavedConnection,
    pub session: Session,
}

pub fn driver(kind: DriverKind) -> Option<Arc<dyn Driver>> {
    match kind {
        DriverKind::MySql => Some(Arc::new(honcho_db::mysql::MySqlDriver)),
        #[cfg(test)]
        DriverKind::Fake => Some(Arc::new(honcho_db::fake::FakeDriver::default())),
        _ => None,
    }
}

pub fn driver_name(kind: DriverKind) -> &'static str {
    match kind {
        DriverKind::MySql => "MySQL / MariaDB",
        DriverKind::SqlServer => "SQL Server",
        DriverKind::Postgres => "PostgreSQL",
        DriverKind::Sqlite => "SQLite",
        DriverKind::Fake => "Fake",
    }
}

pub fn default_port(kind: DriverKind) -> Option<u16> {
    match kind {
        DriverKind::MySql => Some(3306),
        DriverKind::SqlServer => Some(1433),
        DriverKind::Postgres => Some(5432),
        DriverKind::Sqlite | DriverKind::Fake => None,
    }
}

pub fn color(tag: ColorTag) -> Hsla {
    match tag {
        ColorTag::Red => c(0xe5534b),
        ColorTag::Orange => c(0xe08c3c),
        ColorTag::Yellow => c(0xd9c04a),
        ColorTag::Green => c(0x57ab5a),
        ColorTag::Blue => c(0x4c8fe0),
        ColorTag::Purple => c(0xa371f7),
        ColorTag::Gray => c(0x8b8f96),
    }
}
