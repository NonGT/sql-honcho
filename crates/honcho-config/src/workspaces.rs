//! Workspaces: named sets of saved connections, so one customer's
//! databases can be looked at without the rest.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{write_atomically, ColorTag, ConfigError};

const FILE_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SavedWorkspace {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub color: Option<ColorTag>,
    /// Ids of saved connections. One can be in several workspaces.
    #[serde(default)]
    pub connections: Vec<String>,
}

impl SavedWorkspace {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.into(),
            color: None,
            connections: Vec::new(),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct File {
    version: u32,
    #[serde(default)]
    active: Option<String>,
    #[serde(default)]
    workspaces: Vec<SavedWorkspace>,
}

pub struct WorkspaceStore {
    path: PathBuf,
    workspaces: Vec<SavedWorkspace>,
    /// None shows every connection.
    active: Option<String>,
}

impl WorkspaceStore {
    pub fn empty(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            workspaces: Vec::new(),
            active: None,
        }
    }

    /// A missing file is no workspaces. A broken one is an error, so the
    /// next save doesn't replace it.
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
        let mut store = Self {
            path,
            workspaces: file.workspaces,
            active: file.active,
        };
        if store.active().is_none() {
            store.active = None;
        }
        Ok(store)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn workspaces(&self) -> &[SavedWorkspace] {
        &self.workspaces
    }

    pub fn get(&self, id: &str) -> Option<&SavedWorkspace> {
        self.workspaces.iter().find(|w| w.id == id)
    }

    pub fn active(&self) -> Option<&SavedWorkspace> {
        self.get(self.active.as_deref()?)
    }

    /// An unknown id shows everything.
    pub fn set_active(&mut self, id: Option<&str>) {
        self.active = id.filter(|id| self.get(id).is_some()).map(str::to_string);
    }

    /// Whether a connection shows in the active workspace.
    pub fn shows(&self, connection_id: &str) -> bool {
        self.active()
            .is_none_or(|w| w.connections.iter().any(|c| c == connection_id))
    }

    /// Puts a connection in the active workspace, if there is one.
    pub fn add_to_active(&mut self, connection_id: &str) {
        let Some(id) = self.active.clone() else {
            return;
        };
        if let Some(w) = self.workspaces.iter_mut().find(|w| w.id == id) {
            if !w.connections.iter().any(|c| c == connection_id) {
                w.connections.push(connection_id.to_string());
            }
        }
    }

    /// Replaces the workspace with the same id, or adds it at the end.
    pub fn upsert(&mut self, workspace: SavedWorkspace) {
        match self.workspaces.iter_mut().find(|w| w.id == workspace.id) {
            Some(slot) => *slot = workspace,
            None => self.workspaces.push(workspace),
        }
    }

    pub fn remove(&mut self, id: &str) -> Option<SavedWorkspace> {
        let ix = self.workspaces.iter().position(|w| w.id == id)?;
        if self.active.as_deref() == Some(id) {
            self.active = None;
        }
        Some(self.workspaces.remove(ix))
    }

    /// Drops connections that aren't saved any more. True if any were.
    pub fn retain_connections(&mut self, saved: &HashSet<String>) -> bool {
        let mut changed = false;
        for w in &mut self.workspaces {
            let before = w.connections.len();
            w.connections.retain(|c| saved.contains(c));
            changed |= w.connections.len() != before;
        }
        changed
    }

    pub fn save(&self) -> Result<(), ConfigError> {
        let file = File {
            version: FILE_VERSION,
            active: self.active.clone(),
            workspaces: self.workspaces.clone(),
        };
        let json = serde_json::to_string_pretty(&file).expect("workspaces serialize");
        write_atomically(&self.path, &json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saves_loads_and_scopes_connections() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspaces.json");
        let mut store = WorkspaceStore::load(&path).unwrap();
        assert!(store.workspaces().is_empty());
        assert!(store.shows("a"), "no workspace shows everything");

        let mut acme = SavedWorkspace::new("Acme");
        acme.color = Some(ColorTag::Blue);
        acme.connections = vec!["a".into(), "b".into()];
        let globex = SavedWorkspace {
            connections: vec!["b".into()],
            ..SavedWorkspace::new("Globex")
        };
        store.upsert(acme.clone());
        store.upsert(globex.clone());
        store.set_active(Some(&globex.id));
        assert!(!store.shows("a"));
        assert!(store.shows("b"));

        store.add_to_active("c");
        store.add_to_active("c");
        store.save().unwrap();

        let mut loaded = WorkspaceStore::load(&path).unwrap();
        assert_eq!(loaded.active().map(|w| w.name.as_str()), Some("Globex"));
        assert_eq!(loaded.active().unwrap().connections, ["b", "c"]);
        assert_eq!(loaded.get(&acme.id), Some(&acme));

        let saved: HashSet<String> = ["a".to_string(), "c".to_string()].into();
        assert!(loaded.retain_connections(&saved));
        assert_eq!(loaded.get(&acme.id).unwrap().connections, ["a"]);
        assert!(!loaded.retain_connections(&saved));

        loaded.remove(&globex.id);
        assert!(loaded.active().is_none());
        assert!(loaded.shows("b"));
    }

    #[test]
    fn a_missing_active_workspace_shows_everything() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspaces.json");
        std::fs::write(&path, r#"{"version":1,"active":"gone","workspaces":[]}"#).unwrap();
        let store = WorkspaceStore::load(&path).unwrap();
        assert!(store.active().is_none());
        assert!(store.shows("x"));
    }

    #[test]
    fn a_broken_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspaces.json");
        std::fs::write(&path, "{nope").unwrap();
        assert!(WorkspaceStore::load(&path).is_err());
    }
}
