//! Against files built from docker/seed/sqlite.sql; no server needed.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, OnceLock};

use super::*;
use crate::conformance::{Fixture, Sql};
use crate::{DbRuntime, QueryEvent, Session, SessionOptions};

const SEED: &str = include_str!("../../../../docker/seed/sqlite.sql");

/// A fresh seeded file, so tests that write don't see each other.
fn seeded() -> PathBuf {
    static DIR: OnceLock<tempfile::TempDir> = OnceLock::new();
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let dir = DIR.get_or_init(|| tempfile::tempdir().unwrap());
    let path = dir
        .path()
        .join(format!("app{}.db", NEXT.fetch_add(1, Ordering::SeqCst)));
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch(SEED)
        .unwrap();
    path
}

fn config_for(path: &Path) -> ConnectConfig {
    ConnectConfig {
        host: path.display().to_string(),
        ..Default::default()
    }
}

fn sqlite() -> Fixture {
    Fixture {
        enabled: true,
        driver: Arc::new(SqliteDriver),
        config: config_for(&seeded()),
        bad_auth: None,
        databases: &["main"],
        routines: false,
        seed_scope: Scope {
            database: Some("main".into()),
            schema: None,
        },
        can_switch_database: false,
        sql: Sql {
            types: (
                "SELECT 42 AS n, 12.5 AS d, 'สวัสดี 😀' AS t, NULL AS z, x'0a0b' AS b".into(),
                vec![
                    Value::Int(42),
                    Value::Float(12.5),
                    Value::Text("สวัสดี 😀".into()),
                    Value::Null,
                    Value::Bytes(vec![10, 11]),
                ],
            ),
            rows: |n| {
                format!(
                    "WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM c WHERE i < {n}) \
                     SELECT i FROM c"
                )
            },
            two_results: "SELECT 1; SELECT 2".into(),
            rows_affected: (
                "CREATE TEMP TABLE t (x INT); INSERT INTO t VALUES (1), (2), (3)".into(),
                3,
            ),
            syntax_error: "SELEC 1".into(),
            warning: None,
            // counts forever
            sleep: "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c) \
                    SELECT max(x) FROM c"
                .into(),
        },
    }
}

crate::conformance_tests!(sqlite());

async fn session_on(rt: &DbRuntime, config: ConnectConfig) -> Session {
    rt.connect(Arc::new(SqliteDriver), config, SessionOptions::default())
        .await
        .unwrap()
}

async fn run(s: &Session, sql: &str) -> Vec<QueryEvent> {
    let mut handle = s.execute(sql);
    let mut events = Vec::new();
    while let Some(e) = handle.next().await {
        events.push(e);
    }
    events
}

fn rows_of(events: &[QueryEvent]) -> Vec<Vec<Value>> {
    events
        .iter()
        .filter_map(|e| match e {
            QueryEvent::Rows(r) => Some(r.clone()),
            _ => None,
        })
        .flatten()
        .collect()
}

fn finished_ok(events: &[QueryEvent]) {
    assert!(
        matches!(events.last(), Some(QueryEvent::Finished(Ok(_)))),
        "{events:?}"
    );
}

macro_rules! it {
    ($name:ident, |$s:ident| $body:block) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn $name() {
            let rt = DbRuntime::new().unwrap();
            let $s = session_on(&rt, config_for(&seeded())).await;
            $body
        }
    };
}

it!(reads_values_by_declared_type, |s| {
    let events = run(
        &s,
        "CREATE TEMP TABLE v (b BOOLEAN, d DATE, dt DATETIME, t TIME, j JSON, r REAL);
         INSERT INTO v VALUES (1, '2024-01-31', '2024-01-31 10:20:30', '10:20', '{\"a\":1}', 1.5);
         SELECT * FROM v",
    )
    .await;
    finished_ok(&events);
    assert_eq!(
        rows_of(&events),
        vec![vec![
            Value::Bool(true),
            Value::Date("2024-01-31".into()),
            Value::DateTime("2024-01-31 10:20:30".into()),
            Value::Time("10:20".into()),
            Value::Json("{\"a\":1}".into()),
            Value::Float(1.5),
        ]]
    );
    // Thai text stored in the seed
    let events = run(&s, "SELECT email FROM users WHERE id = 2").await;
    assert_eq!(
        rows_of(&events),
        vec![vec![Value::Text("ข@example.com".into())]]
    );
});

it!(points_errors_at_script_lines, |s| {
    let events = run(
        &s,
        "SELECT 1;\nSELECT 2;\n\nSELECT\n  nope\nFROM users;\nSELECT 3",
    )
    .await;
    assert_eq!(
        rows_of(&events),
        vec![vec![Value::Int(1)], vec![Value::Int(2)]]
    );
    match events.last() {
        Some(QueryEvent::Finished(Err(DbError::Query { line, message, .. }))) => {
            assert_eq!(*line, Some(5));
            assert!(message.contains("nope"), "{message}");
        }
        other => panic!("{other:?}"),
    }
});

it!(runs_trigger_scripts_and_reports_changes, |s| {
    let events = run(
        &s,
        "CREATE TRIGGER users_seen AFTER UPDATE ON users BEGIN
             UPDATE orders SET amount = amount WHERE user_id = NEW.id;
         END;
         UPDATE users SET active = 0 WHERE id = 1;
         SELECT count(*) FROM active_users",
    )
    .await;
    finished_ok(&events);
    let done: Vec<Option<u64>> = events
        .iter()
        .filter_map(|e| match e {
            QueryEvent::Done { rows_affected } => Some(*rows_affected),
            _ => None,
        })
        .collect();
    assert_eq!(done, [None, Some(1), None]);
    assert_eq!(rows_of(&events), vec![vec![Value::Int(1)]]);
    // the seed's trigger fills in placed_at on new orders
    let events = run(
        &s,
        "INSERT INTO orders (user_id, amount) VALUES (2, 1);
         SELECT placed_at IS NOT NULL FROM orders ORDER BY id DESC LIMIT 1",
    )
    .await;
    assert_eq!(rows_of(&events), vec![vec![Value::Int(1)]]);
});

it!(scripts_tables_views_and_triggers, |s| {
    let orders = ObjectRef::new(ObjectKind::Table, "orders").in_database("main");
    let def = s.definition(orders.clone()).await.unwrap();
    assert!(def.starts_with("CREATE TABLE orders ("), "{def}");
    assert!(
        def.ends_with(";\nCREATE INDEX orders_user ON orders (user_id)"),
        "{def}"
    );
    let trigger = ObjectRef::new(ObjectKind::Trigger, "orders_placed").in_database("main");
    let def = s.definition(trigger).await.unwrap();
    assert!(def.ends_with("END"), "{def}");
    let indexes = s.indexes(orders).await.unwrap();
    assert_eq!(
        indexes
            .iter()
            .map(|i| (i.name.as_str(), i.primary))
            .collect::<Vec<_>>(),
        [("PRIMARY KEY", true), ("orders_user", false)]
    );
    let found = s.search_bodies("ORDERS", 10).await.unwrap();
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].name, "orders_placed");
});

it!(lists_attached_databases_without_moving_to_them, |s| {
    let other = seeded();
    finished_ok(&run(&s, &format!("ATTACH '{}' AS analytics", other.display())).await);
    assert_eq!(s.databases().await.unwrap(), ["main", "analytics"]);
    let objects = s
        .objects(
            Scope {
                database: Some("analytics".into()),
                schema: None,
            },
            vec![ObjectKind::View],
        )
        .await
        .unwrap();
    assert_eq!(objects.len(), 1);
    assert_eq!(objects[0].database.as_deref(), Some("analytics"));
    let cols = s
        .columns(ObjectRef::new(ObjectKind::Table, "users").in_database("analytics"))
        .await
        .unwrap();
    assert_eq!(cols.len(), 3);
    assert!(matches!(
        s.use_database("analytics").await,
        Err(DbError::Unsupported(_))
    ));
    s.use_database("main").await.unwrap();
    assert!(s.use_database("nope").await.is_err());
});

it!(knows_when_a_transaction_is_open, |s| {
    assert!(!s.in_transaction().await.unwrap());
    finished_ok(&run(&s, "BEGIN; INSERT INTO users (email) VALUES ('x')").await);
    assert!(s.in_transaction().await.unwrap());
    finished_ok(&run(&s, "ROLLBACK").await);
    assert!(!s.in_transaction().await.unwrap());
});

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_only_files_refuse_writes() {
    let path = seeded();
    let mut conn = SqliteDriver
        .connect(&ConnectConfig {
            read_only: true,
            ..config_for(&path)
        })
        .await
        .unwrap();
    let (tx, _rx) = mpsc::channel(16);
    let mut sink = ResultSink::new(tx);
    let err = conn
        .execute("DELETE FROM orders", &mut sink)
        .await
        .err()
        .unwrap();
    match err {
        DbError::Query { message, .. } => assert!(message.contains("readonly"), "{message}"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn says_what_is_wrong_with_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing.db");
    let err = SqliteDriver
        .connect(&config_for(&missing))
        .await
        .err()
        .unwrap();
    assert!(matches!(err, DbError::Connect(_)), "{err:?}");
    // and doesn't create it
    assert!(!missing.exists());

    let text = dir.path().join("notes.txt");
    std::fs::write(&text, "not a database, just some words in a file").unwrap();
    match SqliteDriver
        .connect(&config_for(&text))
        .await
        .err()
        .unwrap()
    {
        DbError::Connect(m) => assert!(m.contains("not a database"), "{m}"),
        other => panic!("{other:?}"),
    }
    let err = SqliteDriver
        .connect(&config_for(Path::new("")))
        .await
        .err()
        .unwrap();
    assert!(matches!(err, DbError::Connect(_)), "{err:?}");
}

it!(project_round_trip, |s| {
    use crate::project::{self, Manifest, Project, Status};
    let dir = tempfile::tempdir().unwrap();
    let p = Project::init(
        dir.path(),
        Manifest {
            connection: "file".into(),
            database: "main".into(),
        },
    )
    .unwrap();
    let entries = project::status(&s, &p).await.unwrap();
    let names: Vec<_> = entries.iter().map(|e| e.object.name.as_str()).collect();
    assert!(
        names.contains(&"active_users") && names.contains(&"orders_placed"),
        "{names:?}"
    );
    for e in &entries {
        project::pull(&p, DriverKind::Sqlite, e).unwrap();
    }
    for e in &project::status(&s, &p).await.unwrap() {
        assert_eq!(e.status, Status::Same, "{}", e.object.name);
    }

    let file = dir.path().join("views/active_users.sql");
    let text = std::fs::read_to_string(&file).unwrap();
    assert!(text.starts_with("DROP VIEW IF EXISTS"), "{text}");
    std::fs::write(&file, text.replace("WHERE active", "WHERE NOT active")).unwrap();
    let entries = project::status(&s, &p).await.unwrap();
    let edited = entries
        .iter()
        .find(|e| e.object.name == "active_users")
        .unwrap();
    assert_eq!(edited.status, Status::Edited);
    project::apply(&s, &p, DriverKind::Sqlite, edited)
        .await
        .unwrap();
    let events = run(&s, "SELECT count(*) FROM active_users").await;
    assert_eq!(rows_of(&events), vec![vec![Value::Int(0)]]);

    // a broken view fails its trial create and leaves the real one alone
    std::fs::write(&file, text.replace("FROM users", "FROM no_such_table")).unwrap();
    let entries = project::status(&s, &p).await.unwrap();
    let broken = entries
        .iter()
        .find(|e| e.object.name == "active_users")
        .unwrap();
    assert!(project::apply(&s, &p, DriverKind::Sqlite, broken)
        .await
        .is_err());
    let views = s
        .objects(Scope::default(), vec![ObjectKind::View])
        .await
        .unwrap();
    assert_eq!(views.len(), 1, "the trial view stayed: {views:?}");
    let events = run(&s, "SELECT count(*) FROM active_users").await;
    assert_eq!(rows_of(&events), vec![vec![Value::Int(0)]]);
});
