//! A folder of SQL files, one per routine, trigger or view, kept in step
//! with a database: pull writes the server's definitions into files, apply
//! runs changed files against the server. The folder is meant to live in
//! git; the app doesn't run git itself.
//!
//! ```text
//! sql-honcho.json          connection name and database
//! procedures/name.sql     schema.name.sql where the server has schemas
//! functions/name.sql
//! triggers/name.sql
//! views/name.sql
//! .sql-honcho/             per machine, ignores itself
//! ```

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::routine::{self, create_statement, keyword, name_span};
use crate::{DbError, DriverKind, ObjectKind, ObjectRef, QueryEvent, Result, Scope, Session};

pub const MANIFEST: &str = "sql-honcho.json";
const STATE_DIR: &str = ".sql-honcho";
const LAST_STATUS: &str = "last-status.json";
const BASELINE: &str = "baseline.json";

pub const KINDS: [ObjectKind; 4] = [
    ObjectKind::Function,
    ObjectKind::View,
    ObjectKind::Procedure,
    ObjectKind::Trigger,
];

fn folder(kind: ObjectKind) -> Option<&'static str> {
    Some(match kind {
        ObjectKind::Procedure => "procedures",
        ObjectKind::Function => "functions",
        ObjectKind::Trigger => "triggers",
        ObjectKind::View => "views",
        _ => return None,
    })
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    /// The saved connection's name: ids are per machine, names travel with
    /// the repository.
    pub connection: String,
    pub database: String,
}

/// What a file and the server held when they were last pulled or applied,
/// so a difference can be put down to one side. Hashes of normalised text.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
struct Seen {
    file: u64,
    server: u64,
}

#[derive(Default, Serialize, Deserialize)]
struct Baseline {
    database: String,
    seen: BTreeMap<String, Seen>,
}

/// A file's kind, schema, object name and text.
type FileEntry = (ObjectKind, Option<String>, String, String);

#[derive(Clone, Debug)]
pub struct Project {
    pub root: PathBuf,
    pub manifest: Manifest,
}

fn io(path: &Path, e: std::io::Error) -> DbError {
    DbError::Other(format!("{}: {e}", path.display()))
}

impl Project {
    pub fn open(root: &Path) -> Result<Self> {
        let path = root.join(MANIFEST);
        let text = fs::read_to_string(&path).map_err(|e| io(&path, e))?;
        let manifest = serde_json::from_str(&text)
            .map_err(|e| DbError::Other(format!("{}: {e}", path.display())))?;
        Ok(Self {
            root: root.to_path_buf(),
            manifest,
        })
    }

    /// Makes `root` a project, or repoints an existing one.
    pub fn init(root: &Path, manifest: Manifest) -> Result<Self> {
        fs::create_dir_all(root).map_err(|e| io(root, e))?;
        let path = root.join(MANIFEST);
        let text = serde_json::to_string_pretty(&manifest).expect("manifest serialises");
        fs::write(&path, text + "\n").map_err(|e| io(&path, e))?;
        Ok(Self {
            root: root.to_path_buf(),
            manifest,
        })
    }

    /// Where an object's file goes, relative to the root. Dots in names
    /// are escaped, so the one after the schema is unambiguous.
    pub fn file_for(kind: ObjectKind, schema: Option<&str>, name: &str) -> Option<PathBuf> {
        let stem = match schema {
            Some(schema) => format!("{}.{}", file_name(schema), file_name(name)),
            None => file_name(name),
        };
        Some(Path::new(folder(kind)?).join(format!("{stem}.sql")))
    }

    /// The objects the folder has files for, with their text.
    fn files(&self) -> Result<Vec<FileEntry>> {
        let mut out = Vec::new();
        for kind in KINDS {
            let dir = self.root.join(folder(kind).unwrap());
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let Some(stem) = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .and_then(|n| n.strip_suffix(".sql"))
                else {
                    continue;
                };
                let text = fs::read_to_string(&path).map_err(|e| io(&path, e))?;
                let (schema, name) = match stem.split_once('.') {
                    Some((schema, name)) => (Some(object_name(schema)), name),
                    None => (None, stem),
                };
                out.push((kind, schema, object_name(name), text));
            }
        }
        Ok(out)
    }

    fn baseline(&self) -> Baseline {
        let path = self.root.join(STATE_DIR).join(BASELINE);
        fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str::<Baseline>(&t).ok())
            // seen against another database means nothing here
            .filter(|b| b.database == self.manifest.database)
            .unwrap_or_default()
    }

    /// The last comparison, to show while it's done again. Stale by
    /// nature: never pull or apply from it.
    pub fn last_status(&self) -> Option<Vec<Entry>> {
        let path = self.root.join(STATE_DIR).join(LAST_STATUS);
        let (database, entries): (String, Vec<Entry>) =
            serde_json::from_str(&fs::read_to_string(path).ok()?).ok()?;
        (database == self.manifest.database).then_some(entries)
    }

    pub fn save_status(&self, entries: &[Entry]) -> Result<()> {
        let dir = self.state_dir()?;
        let path = dir.join(LAST_STATUS);
        let text = serde_json::to_string(&(&self.manifest.database, entries))
            .map_err(|e| DbError::Other(e.to_string()))?;
        fs::write(&path, text).map_err(|e| io(&path, e))
    }

    fn state_dir(&self) -> Result<PathBuf> {
        let dir = self.root.join(STATE_DIR);
        fs::create_dir_all(&dir).map_err(|e| io(&dir, e))?;
        let ignore = dir.join(".gitignore");
        if !ignore.exists() {
            fs::write(&ignore, "*\n").map_err(|e| io(&ignore, e))?;
        }
        Ok(dir)
    }

    fn save_baseline(&self, mut baseline: Baseline) -> Result<()> {
        let dir = self.state_dir()?;
        baseline.database = self.manifest.database.clone();
        let path = dir.join(BASELINE);
        let text = serde_json::to_string_pretty(&baseline).expect("baseline serialises");
        fs::write(&path, text).map_err(|e| io(&path, e))
    }

    fn remember(&self, object: &ObjectRef, file: &str, server: &str) -> Result<()> {
        let kind = object.kind;
        let mut baseline = self.baseline();
        baseline.seen.insert(
            key(kind, object.schema.as_deref(), &object.name),
            Seen {
                file: hash(&normalise(file, kind)),
                server: hash(&normalise(server, kind)),
            },
        );
        self.save_baseline(baseline)
    }
}

fn key(kind: ObjectKind, schema: Option<&str>, name: &str) -> String {
    Project::file_for(kind, schema, name)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default()
}

/// Names go into file names as they are, except characters a file system
/// might not take, which become %XX.
fn file_name(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if c.is_alphanumeric() || matches!(c, '_' | '-' | '$' | ' ') {
            out.push(c);
        } else {
            let mut buf = [0; 4];
            for b in c.encode_utf8(&mut buf).bytes() {
                out.push_str(&format!("%{b:02X}"));
            }
        }
    }
    out
}

fn object_name(stem: &str) -> String {
    let bytes = stem.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&stem[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// FNV-1a: stable across builds, unlike the std hasher.
fn hash(s: &str) -> u64 {
    s.bytes().fold(0xcbf29ce484222325, |h, b| {
        (h ^ b as u64).wrapping_mul(0x100000001b3)
    })
}

/// A server definition as it goes into a file: DEFINER is dropped, since
/// the account differs between servers and creating as yourself is what
/// you want from a file, and the name isn't tied to a database, so the
/// same file applies to dev and prod.
pub fn file_text(driver: DriverKind, object: &ObjectRef, definition: &str) -> String {
    let unqualified = ObjectRef {
        database: None,
        schema: None,
        ..object.clone()
    };
    routine::script(driver, &unqualified, &without_definer(definition))
}

fn without_definer(definition: &str) -> String {
    let upper = definition.to_ascii_uppercase();
    let Some(at) = upper.find("DEFINER=") else {
        return definition.to_string();
    };
    // only in the header, before the body could mention it
    if upper[..at].contains(" AS ") || upper[..at].contains("BEGIN") {
        return definition.to_string();
    }
    let rest = &definition[at + "DEFINER=".len()..];
    let mut end = 0;
    let bytes = rest.as_bytes();
    while end < bytes.len() && !bytes[end].is_ascii_whitespace() {
        if matches!(bytes[end], b'`' | b'\'' | b'"') {
            let q = bytes[end];
            end += 1;
            while end < bytes.len() && bytes[end] != q {
                end += 1;
            }
        }
        end += 1;
    }
    let tail = rest[end.min(rest.len())..].trim_start();
    format!("{}{}", &definition[..at], tail)
}

/// The part of a definition or file that says what the object does: the
/// CREATE statement from the object's name on, with whitespace at line
/// ends and the statement terminator gone. DEFINER, ALGORITHM, OR REPLACE
/// and the like before the name, and the name's quoting and database,
/// don't count.
pub fn normalise(text: &str, kind: ObjectKind) -> String {
    // a definition from the server is one statement, semicolons and all;
    // a MySQL file is a script with DELIMITER lines around it
    let code = &text[routine::code_start(text)..];
    let starts_create = code
        .split_whitespace()
        .next()
        .is_some_and(|w| w.eq_ignore_ascii_case("CREATE"));
    let create = if starts_create {
        code
    } else {
        create_statement(DriverKind::MySql, text).unwrap_or(text)
    };
    let create = create.replace("\r\n", "\n");
    let tidy = |s: &str| {
        s.trim()
            .trim_end_matches(';')
            .trim_end()
            .lines()
            .map(str::trim_end)
            .collect::<Vec<_>>()
            .join("\n")
    };
    let Some((_, start, end)) = keyword(kind).and_then(|k| name_span(&create, k)) else {
        return tidy(&create);
    };
    let name = create[start..end]
        .rsplit('.')
        .next()
        .unwrap_or_default()
        .trim_matches(['`', '"', '[', ']'])
        .to_lowercase();
    format!("{name}{}", tidy(&create[end..]))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Status {
    Same,
    /// The file changed since it was last pulled or applied.
    Edited,
    /// The server's copy changed since then.
    ServerChanged,
    /// Both did.
    BothChanged,
    /// They differ and there's no record of which side moved.
    Differs,
    /// A file for an object the server doesn't have.
    FileOnly,
    /// On the server, not pulled into the folder.
    ServerOnly,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Entry {
    pub object: ObjectRef,
    /// Relative to the project root.
    pub path: PathBuf,
    pub file: Option<String>,
    /// The server's definition, as SHOW CREATE gives it.
    pub server: Option<String>,
    pub status: Status,
}

impl Entry {
    /// What pulling would write.
    pub fn pulled(&self, driver: DriverKind) -> Option<String> {
        self.server
            .as_ref()
            .map(|d| file_text(driver, &self.object, d))
    }
}

fn status_of(
    kind: ObjectKind,
    file: Option<&str>,
    server: Option<&str>,
    seen: Option<Seen>,
) -> Status {
    match (file, server) {
        (Some(f), Some(s)) => {
            let (f, s) = (normalise(f, kind), normalise(s, kind));
            if f == s {
                return Status::Same;
            }
            let Some(seen) = seen else {
                return Status::Differs;
            };
            match (hash(&f) != seen.file, hash(&s) != seen.server) {
                // the server rewrote what was applied (views get reformatted)
                (false, false) => Status::Same,
                (true, false) => Status::Edited,
                (false, true) => Status::ServerChanged,
                (true, true) => Status::BothChanged,
            }
        }
        (Some(_), None) => Status::FileOnly,
        (None, _) => Status::ServerOnly,
    }
}

/// Compares the folder with the database, object by object.
pub async fn status(session: &Session, project: &Project) -> Result<Vec<Entry>> {
    let db = &project.manifest.database;
    let objects = session
        .objects(
            Scope {
                database: Some(db.clone()),
                schema: None,
            },
            KINDS.to_vec(),
        )
        .await?;
    type Key = (ObjectKind, Option<String>, String);
    let lower = |s: &str| s.to_lowercase();
    let mut server: HashMap<Key, (ObjectRef, String)> = HashMap::new();
    for o in objects {
        if !KINDS.contains(&o.kind) {
            continue;
        }
        let def = session.definition(o.clone()).await?;
        server.insert(
            (o.kind, o.schema.as_deref().map(lower), lower(&o.name)),
            (o, def),
        );
    }
    let baseline = project.baseline();
    let mut entries = Vec::new();
    for (kind, schema, name, text) in project.files()? {
        let found = server.remove(&(kind, schema.as_deref().map(lower), lower(&name)));
        let object = found
            .as_ref()
            .map(|(o, _)| o.clone())
            .unwrap_or_else(|| ObjectRef {
                schema: schema.clone(),
                ..ObjectRef::new(kind, name.clone()).in_database(db.clone())
            });
        let def = found.map(|(_, d)| d);
        entries.push(Entry {
            path: Project::file_for(kind, schema.as_deref(), &name).unwrap(),
            status: status_of(
                kind,
                Some(&text),
                def.as_deref(),
                baseline
                    .seen
                    .get(&key(kind, schema.as_deref(), &name))
                    .copied(),
            ),
            object,
            file: Some(text),
            server: def,
        });
    }
    for ((kind, ..), (object, def)) in server {
        entries.push(Entry {
            path: Project::file_for(kind, object.schema.as_deref(), &object.name).unwrap(),
            status: Status::ServerOnly,
            object,
            file: None,
            server: Some(def),
        });
    }
    order(&mut entries);
    Ok(entries)
}

/// Writes the server's definition into the entry's file.
pub fn pull(project: &Project, driver: DriverKind, entry: &Entry) -> Result<()> {
    let Some(def) = &entry.server else {
        return Ok(());
    };
    let text = file_text(driver, &entry.object, def);
    let path = project.root.join(&entry.path);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| io(dir, e))?;
    }
    fs::write(&path, &text).map_err(|e| io(&path, e))?;
    project.remember(&entry.object, &text, def)
}

async fn run(session: &Session, sql: String) -> Result<()> {
    let mut handle = session.execute(sql);
    let mut result = Ok(());
    while let Some(event) = handle.next().await {
        if let QueryEvent::Finished(r) = event {
            result = r.map(|_| ());
        }
    }
    result
}

/// Runs the entry's file against the database. Where the file drops the
/// object before creating it, its CREATE is tried under a throwaway name
/// first, so a file that won't create leaves the object alone; and if the
/// create still fails after the drop, the server's previous definition is
/// put back. Returns the definition the server now has.
pub async fn apply(
    session: &Session,
    project: &Project,
    driver: DriverKind,
    entry: &Entry,
) -> Result<String> {
    let Some(file) = &entry.file else {
        return Err(DbError::Other("no file to apply".into()));
    };
    session
        .use_database(project.manifest.database.clone())
        .await?;
    let unqualified = ObjectRef {
        database: None,
        schema: None,
        ..entry.object.clone()
    };
    if let Some((create, drop)) = routine::trial(driver, &unqualified, file) {
        let tried = run(session, create).await;
        // the trial can fail after creating, as SQLite's view check does
        let _ = run(session, drop).await;
        tried?;
    }
    if let Err(e) = run(session, file.clone()).await {
        if let Some(old) = &entry.server {
            if session.definition(entry.object.clone()).await.is_err() {
                let restore = routine::restore_script(driver, &unqualified, old);
                let _ = run(session, restore).await;
            }
        }
        return Err(e);
    }
    let now = session.definition(entry.object.clone()).await?;
    project.remember(&entry.object, file, &now)?;
    Ok(now)
}

/// The order to apply in: functions first, since views and routines may
/// call them, then views, each after the views it reads from, then
/// procedures and triggers.
pub fn order(entries: &mut Vec<Entry>) {
    let rank = |k: ObjectKind| KINDS.iter().position(|&x| x == k).unwrap_or(KINDS.len());
    entries.sort_by(|a, b| {
        (rank(a.object.kind), a.object.name.to_lowercase())
            .cmp(&(rank(b.object.kind), b.object.name.to_lowercase()))
    });
    // views: move each after any view its text names
    let views: Vec<usize> = (0..entries.len())
        .filter(|&i| entries[i].object.kind == ObjectKind::View)
        .collect();
    let (Some(&first), Some(&last)) = (views.first(), views.last()) else {
        return;
    };
    let mut pending: Vec<Entry> = entries.drain(first..=last).collect();
    let mut sorted: Vec<Entry> = Vec::new();
    while !pending.is_empty() {
        let ready = pending
            .iter()
            .position(|v| {
                let text = v.file.as_deref().or(v.server.as_deref()).unwrap_or("");
                !pending.iter().any(|other| {
                    other.object.name != v.object.name && mentions(text, &other.object.name)
                })
            })
            // a cycle: keep the rest as they are
            .unwrap_or(0);
        sorted.push(pending.remove(ready));
    }
    entries.splice(first..first, sorted);
}

/// Whether SQL text uses a name as a whole word, past the CREATE header.
fn mentions(text: &str, name: &str) -> bool {
    let body = text.to_lowercase();
    let body = body.find(" as ").map_or(body.as_str(), |i| &body[i..]);
    let name = name.to_lowercase();
    body.match_indices(&name).any(|(i, _)| {
        let word = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '$');
        !word(body[..i].chars().next_back()) && !word(body[i + name.len()..].chars().next())
    })
}

#[cfg(test)]
mod tests;
