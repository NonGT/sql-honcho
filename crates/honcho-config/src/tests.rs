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
fn duplicate_and_remove() {
    let dir = tempfile::tempdir().unwrap();
    let secrets = MemorySecrets::default();
    let mut store = ConnectionStore::load(dir.path().join("c.json")).unwrap();
    let a = mysql("a");
    a.set_password(&secrets, Some("pw")).unwrap();
    store.upsert(a.clone());
    store.upsert(mysql("b"));

    let copy = store.duplicate(&a.id).unwrap().clone();
    assert_ne!(copy.id, a.id);
    assert_eq!(copy.name, "a copy");
    assert_eq!(copy.host, a.host);
    let names: Vec<&str> = store
        .connections()
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(names, ["a", "a copy", "b"]);
    assert_eq!(copy.password(&secrets).unwrap(), None);
    a.copy_secrets_to(&copy, &secrets).unwrap();
    assert_eq!(copy.password(&secrets).unwrap().as_deref(), Some("pw"));

    assert_eq!(store.remove(&a.id).unwrap().id, a.id);
    assert_eq!(store.connections().len(), 2);
    assert!(store.remove("missing").is_none());
    assert!(store.duplicate("missing").is_none());
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

#[test]
fn ssh_settings_and_secret() {
    let dir = tempfile::tempdir().unwrap();
    let secrets = MemorySecrets::default();
    let path = dir.path().join("c.json");
    let mut store = ConnectionStore::load(&path).unwrap();
    let c = SavedConnection {
        ssh: Some(SavedSsh {
            host: "bastion".into(),
            user: "ops".into(),
            auth: honcho_db::tunnel::SshAuth::Password,
            ..Default::default()
        }),
        ..mysql("via ssh")
    };
    c.set_ssh_secret(&secrets, Some("ssh pw")).unwrap();
    store.upsert(c.clone());
    store.save().unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        text.contains("bastion") && !text.contains("ssh pw"),
        "{text}"
    );

    let loaded = ConnectionStore::load(&path).unwrap();
    let config = loaded.connections()[0].connect_config(&secrets).unwrap();
    let ssh = config.ssh.unwrap();
    assert_eq!(ssh.host, "bastion");
    assert_eq!(ssh.user.as_deref(), Some("ops"));
    assert_eq!(ssh.secret.as_deref(), Some("ssh pw"));

    c.forget_secrets(&secrets).unwrap();
    assert_eq!(c.ssh_secret(&secrets).unwrap(), None);
}
