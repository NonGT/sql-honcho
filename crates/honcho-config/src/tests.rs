use super::*;

fn mysql(name: &str) -> SavedConnection {
    SavedConnection {
        host: "db.example.com".into(),
        port: Some(3307),
        user: "app".into(),
        database: Some("shop".into()),
        color: Some(ColorTag::Red),
        read_only: true,
        ..SavedConnection::new(name, DriverKind::MySql)
    }
}

#[test]
fn missing_file_is_an_empty_store() {
    let dir = tempfile::tempdir().unwrap();
    let store = ConnectionStore::load(dir.path().join("nope/connections.json")).unwrap();
    assert!(store.connections().is_empty());
}

#[test]
fn saves_and_loads_without_passwords() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sub/connections.json");
    let secrets = MemorySecrets::default();

    let mut store = ConnectionStore::load(&path).unwrap();
    let prod = mysql("ลูกค้า prod");
    prod.set_password(&secrets, Some("s3cret!")).unwrap();
    store.upsert(prod.clone());
    store.save().unwrap();

    let text = std::fs::read_to_string(&path).unwrap();
    assert!(!text.contains("s3cret"), "{text}");
    assert!(text.contains("\"driver\": \"mysql\""), "{text}");

    let loaded = ConnectionStore::load(&path).unwrap();
    assert_eq!(loaded.connections(), std::slice::from_ref(&prod));
    let config = loaded.connections()[0].connect_config(&secrets).unwrap();
    assert_eq!(config.password.as_deref(), Some("s3cret!"));
    assert_eq!(config.port, Some(3307));
    assert!(config.read_only);

    // no temp files left behind
    let files: Vec<_> = std::fs::read_dir(path.parent().unwrap()).unwrap().collect();
    assert_eq!(files.len(), 1);
}

#[test]
fn upsert_replaces_by_id() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = ConnectionStore::load(dir.path().join("c.json")).unwrap();
    let mut a = mysql("a");
    store.upsert(a.clone());
    store.upsert(mysql("b"));
    a.name = "renamed".into();
    store.upsert(a.clone());
    let names: Vec<&str> = store
        .connections()
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(names, ["renamed", "b"]);
    assert_eq!(store.get(&a.id), Some(&a));
}

#[test]
fn duplicate_and_remove_carry_the_password() {
    let dir = tempfile::tempdir().unwrap();
    let secrets = MemorySecrets::default();
    let mut store = ConnectionStore::load(dir.path().join("c.json")).unwrap();
    let a = mysql("a");
    a.set_password(&secrets, Some("pw")).unwrap();
    store.upsert(a.clone());
    store.upsert(mysql("b"));

    let copy = store.duplicate(&a.id, &secrets).unwrap().unwrap().clone();
    assert_ne!(copy.id, a.id);
    assert_eq!(copy.name, "a copy");
    assert_eq!(copy.host, a.host);
    assert_eq!(copy.password(&secrets).unwrap().as_deref(), Some("pw"));
    let names: Vec<&str> = store
        .connections()
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(names, ["a", "a copy", "b"]);

    store.remove(&a.id, &secrets).unwrap().unwrap();
    assert_eq!(a.password(&secrets).unwrap(), None);
    assert_eq!(copy.password(&secrets).unwrap().as_deref(), Some("pw"));
    assert!(store.remove("missing", &secrets).unwrap().is_none());
    assert!(store.duplicate("missing", &secrets).unwrap().is_none());
}

#[test]
fn broken_or_newer_files_are_errors() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("c.json");
    std::fs::write(&path, "{ not json").unwrap();
    assert!(matches!(
        ConnectionStore::load(&path),
        Err(ConfigError::Parse { .. })
    ));
    std::fs::write(&path, r#"{"version": 99, "connections": []}"#).unwrap();
    let err = ConnectionStore::load(&path).err().unwrap();
    assert!(err.to_string().contains("newer"), "{err}");
}

#[test]
fn old_entries_get_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("c.json");
    std::fs::write(
        &path,
        r#"{"version": 1, "connections": [{"id": "x", "name": "local", "driver": "postgres"}]}"#,
    )
    .unwrap();
    let store = ConnectionStore::load(&path).unwrap();
    let c = &store.connections()[0];
    assert_eq!(c.driver, DriverKind::Postgres);
    assert!(!c.read_only);
    assert_eq!(c.color, None);
}

#[test]
fn default_path_is_in_the_config_dir() {
    let path = ConnectionStore::default_path().unwrap();
    assert!(path.ends_with("connections.json"));
    assert!(path.to_string_lossy().contains("sql-honcho"));
}
