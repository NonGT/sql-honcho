//! Saved connections. The JSON file holds everything but passwords, which
//! go to the OS credential store.

mod secrets;

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use honcho_db::{ConnectConfig, DriverKind, TlsConfig};
use serde::{Deserialize, Serialize};

pub use secrets::{Keyring, MemorySecrets, SecretError, Secrets};

const FILE_VERSION: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{}: {message}", path.display())]
    Parse { path: PathBuf, message: String },
    #[error(transparent)]
    Secret(#[from] SecretError),
}

/// Shown on tabs and the status bar, so a production connection stands out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ColorTag {
    Red,
    Orange,
    Yellow,
    Green,
    Blue,
    Purple,
    Gray,
}

impl ColorTag {
    pub const ALL: [ColorTag; 7] = [
        ColorTag::Red,
        ColorTag::Orange,
        ColorTag::Yellow,
        ColorTag::Green,
        ColorTag::Blue,
        ColorTag::Purple,
        ColorTag::Gray,
    ];
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SavedConnection {
    pub id: String,
    pub name: String,
    pub driver: DriverKind,
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub database: Option<String>,
    #[serde(default)]
    pub color: Option<ColorTag>,
    #[serde(default)]
    pub read_only: bool,
    /// Reopen the session when the server has closed it.
    #[serde(default)]
    pub auto_reconnect: bool,
    #[serde(default)]
    pub tls: TlsConfig,
    #[serde(default)]
    pub options: BTreeMap<String, String>,
}

impl SavedConnection {
    pub fn new(name: impl Into<String>, driver: DriverKind) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.into(),
            driver,
            host: String::new(),
            port: None,
            user: String::new(),
            database: None,
            color: None,
            read_only: false,
            auto_reconnect: false,
            tls: TlsConfig::default(),
            options: BTreeMap::new(),
        }
    }

    pub fn password_key(&self) -> String {
        format!("{}/password", self.id)
    }

    pub fn password(&self, secrets: &dyn Secrets) -> Result<Option<String>, SecretError> {
        secrets.get(&self.password_key())
    }

    /// Stores the password, or forgets it when `None`.
    pub fn set_password(
        &self,
        secrets: &dyn Secrets,
        password: Option<&str>,
    ) -> Result<(), SecretError> {
        match password {
            Some(p) => secrets.set(&self.password_key(), p),
            None => secrets.delete(&self.password_key()),
        }
    }

    pub fn copy_password_to(
        &self,
        other: &SavedConnection,
        secrets: &dyn Secrets,
    ) -> Result<(), SecretError> {
        match self.password(secrets)? {
            Some(p) => other.set_password(secrets, Some(&p)),
            None => Ok(()),
        }
    }

    pub fn connect_config(&self, secrets: &dyn Secrets) -> Result<ConnectConfig, SecretError> {
        Ok(ConnectConfig {
            host: self.host.clone(),
            port: self.port,
            user: self.user.clone(),
            password: self.password(secrets)?,
            database: self.database.clone(),
            read_only: self.read_only,
            tls: self.tls.clone(),
            options: self.options.clone(),
            ..Default::default()
        })
    }
}

#[derive(Serialize, Deserialize)]
struct File {
    version: u32,
    connections: Vec<SavedConnection>,
}

pub struct ConnectionStore {
    path: PathBuf,
    connections: Vec<SavedConnection>,
}

impl ConnectionStore {
    /// `connections.json` in the platform config dir.
    pub fn default_path() -> Option<PathBuf> {
        directories::ProjectDirs::from("", "", "sql-honcho")
            .map(|d| d.config_dir().join("connections.json"))
    }

    pub fn empty(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            connections: Vec::new(),
        }
    }

    /// A missing file is an empty store. A broken one is an error, so it
    /// doesn't get overwritten by the next save.
    pub fn load(path: impl Into<PathBuf>) -> Result<Self, ConfigError> {
        let path = path.into();
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::empty(path)),
            Err(source) => return Err(ConfigError::Io { path, source }),
        };
        let parse_error = |message: String| ConfigError::Parse {
            path: path.clone(),
            message,
        };
        let file: File = serde_json::from_str(&text).map_err(|e| parse_error(e.to_string()))?;
        if file.version > FILE_VERSION {
            return Err(parse_error(format!(
                "written by a newer sql-honcho (format {})",
                file.version
            )));
        }
        Ok(Self {
            path,
            connections: file.connections,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn connections(&self) -> &[SavedConnection] {
        &self.connections
    }

    pub fn get(&self, id: &str) -> Option<&SavedConnection> {
        self.connections.iter().find(|c| c.id == id)
    }

    /// Replaces the connection with the same id, or adds it at the end.
    pub fn upsert(&mut self, connection: SavedConnection) {
        match self.connections.iter_mut().find(|c| c.id == connection.id) {
            Some(slot) => *slot = connection,
            None => self.connections.push(connection),
        }
    }

    /// Adds a copy right after the original. The password isn't copied:
    /// keyring calls can block on a system prompt, so callers do that where
    /// it can't freeze anything (see [`SavedConnection::copy_password_to`]).
    pub fn duplicate(&mut self, id: &str) -> Option<&SavedConnection> {
        let ix = self.connections.iter().position(|c| c.id == id)?;
        let original = &self.connections[ix];
        let mut copy = original.clone();
        copy.id = uuid::Uuid::new_v4().to_string();
        copy.name = format!("{} copy", original.name);
        self.connections.insert(ix + 1, copy);
        self.connections.get(ix + 1)
    }

    /// Removes the connection. Its password is left for the caller to
    /// delete, for the same reason as in [`duplicate`](Self::duplicate).
    pub fn remove(&mut self, id: &str) -> Option<SavedConnection> {
        let ix = self.connections.iter().position(|c| c.id == id)?;
        Some(self.connections.remove(ix))
    }

    /// Writes to a temp file and renames it over the old one, so a crash
    /// mid-save can't leave half a file.
    pub fn save(&self) -> Result<(), ConfigError> {
        let io = |source| ConfigError::Io {
            path: self.path.clone(),
            source,
        };
        let dir = self.path.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(dir).map_err(io)?;
        let file = File {
            version: FILE_VERSION,
            connections: self.connections.clone(),
        };
        let json = serde_json::to_string_pretty(&file).expect("connections serialize");
        let mut tmp = tempfile_in(dir).map_err(io)?;
        tmp.1.write_all(json.as_bytes()).map_err(io)?;
        tmp.1.sync_all().map_err(io)?;
        drop(tmp.1);
        std::fs::rename(&tmp.0, &self.path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp.0);
            io(e)
        })
    }
}

fn tempfile_in(dir: &Path) -> std::io::Result<(PathBuf, std::fs::File)> {
    let path = dir.join(format!(".connections-{}.tmp", uuid::Uuid::new_v4()));
    let file = std::fs::File::create(&path)?;
    Ok((path, file))
}

#[cfg(test)]
mod tests;
