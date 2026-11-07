//! Run against the server from docker/compose.yml with `HONCHO_IT=1`.

use std::sync::Arc;

use super::*;
use crate::conformance::{Fixture, Sql};
use crate::{DbRuntime, QueryEvent, Session, SessionOptions, Value};

fn enabled() -> bool {
    std::env::var_os("HONCHO_IT").is_some()
}

fn config() -> ConnectConfig {
    ConnectConfig {
        host: "127.0.0.1".into(),
        port: Some(35432),
        user: "honcho".into(),
        password: Some("Honcho_test1".into()),
        database: Some("app".into()),
        ..Default::default()
    }
}

fn postgres() -> Fixture {
    Fixture {
        enabled: enabled(),
        driver: Arc::new(PostgresDriver),
        config: config(),
        bad_auth: ConnectConfig {
            password: Some("wrong".into()),
            ..config()
        },
        seed_scope: Scope {
            database: Some("app".into()),
            schema: Some("public".into()),
        },
        can_switch_database: true,
        sql: Sql {
            types: (
                "SELECT 42 AS n, 12.50::numeric(12, 2) AS d, 'สวัสดี 😀' AS t, \
                 NULL AS z, DATE '2024-01-31' AS day"
                    .into(),
                vec![
                    Value::Int(42),
                    Value::Decimal("12.50".into()),
                    Value::Text("สวัสดี 😀".into()),
                    Value::Null,
                    Value::Date("2024-01-31".into()),
                ],
            ),
            rows: |n| format!("SELECT i FROM generate_series(1, {n}) AS i"),
            two_results: "SELECT 1; SELECT 2".into(),
            rows_affected: (
                "CREATE TEMP TABLE t (x INT); INSERT INTO t VALUES (1), (2), (3)".into(),
                3,
            ),
            syntax_error: "SELEC 1".into(),
            warning: Some("DO $$ BEGIN RAISE WARNING 'hello'; END $$".into()),
            sleep: "SELECT pg_sleep(60)".into(),
        },
    }
}

crate::conformance_tests!(postgres());

async fn session(rt: &DbRuntime) -> Session {
    rt.connect(
        Arc::new(PostgresDriver),
        config(),
        SessionOptions::default(),
    )
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

fn messages(events: &[QueryEvent]) -> Vec<(MessageLevel, String)> {
    events
        .iter()
        .filter_map(|e| match e {
            QueryEvent::Message(m) => Some((m.level, m.text.clone())),
            _ => None,
        })
        .collect()
}

macro_rules! it {
    ($name:ident, |$s:ident| $body:block) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn $name() {
            if !enabled() {
                eprintln!("skipped: test database not enabled");
                return;
            }
            let rt = DbRuntime::new().unwrap();
            let $s = session(&rt).await;
            $body
        }
    };
}

it!(decodes_postgres_types, |s| {
    let events = run(
        &s,
        "SELECT true, 1.5::float8, '\\x0a0b'::bytea, '{\"a\": 1}'::jsonb, \
         TIMESTAMP '2024-01-31 10:20:30.5', TIME '10:20:30', \
         'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11'::uuid, ARRAY[1, 2], 9000000000::int8",
    )
    .await;
    assert!(
        matches!(events.last(), Some(QueryEvent::Finished(Ok(_)))),
        "{events:?}"
    );
    assert_eq!(
        rows_of(&events),
        vec![vec![
            Value::Bool(true),
            Value::Float(1.5),
            Value::Bytes(vec![10, 11]),
            Value::Json("{\"a\": 1}".into()),
            Value::DateTime("2024-01-31 10:20:30.5".into()),
            Value::Time("10:20:30".into()),
            Value::Other("a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11".into()),
            Value::Other("{1,2}".into()),
            Value::Int(9_000_000_000),
        ]]
    );
});

it!(reports_notices_in_order, |s| {
    let events = run(
        &s,
        "DO $$ BEGIN RAISE NOTICE 'one'; END $$; SELECT 1; \
         DO $$ BEGIN RAISE NOTICE 'สอง'; END $$",
    )
    .await;
    assert!(
        matches!(events.last(), Some(QueryEvent::Finished(Ok(_)))),
        "{events:?}"
    );
    assert_eq!(
        messages(&events),
        [
            (MessageLevel::Info, "one".to_string()),
            (MessageLevel::Info, "สอง".to_string()),
        ]
    );
    let first_message = events
        .iter()
        .position(|e| matches!(e, QueryEvent::Message(_)));
    let columns = events
        .iter()
        .position(|e| matches!(e, QueryEvent::Columns(_)));
    assert!(first_message < columns, "{events:?}");
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
        Some(QueryEvent::Finished(Err(DbError::Query { line, code, .. }))) => {
            assert_eq!(*line, Some(5));
            assert_eq!(code.as_deref(), Some("42703"));
        }
        other => panic!("{other:?}"),
    }
});

it!(lists_schemas_and_scripts_objects, |s| {
    let schemas = s.schemas("app").await.unwrap();
    assert_eq!(schemas, ["public"]);

    let orders = ObjectRef::new(ObjectKind::Table, "orders")
        .in_database("app")
        .in_schema("public");
    let def = s.definition(orders.clone()).await.unwrap();
    assert!(def.starts_with("CREATE TABLE public.orders ("), "{def}");
    assert!(def.contains("amount numeric(12,2) NOT NULL"), "{def}");
    assert!(def.contains("PRIMARY KEY (id)"), "{def}");
    assert!(def.contains("REFERENCES users(id)"), "{def}");

    let view = ObjectRef::new(ObjectKind::View, "active_users")
        .in_database("app")
        .in_schema("public");
    let def = s.definition(view).await.unwrap();
    assert!(
        def.starts_with("CREATE OR REPLACE VIEW public.active_users AS\n"),
        "{def}"
    );

    let indexes = s.indexes(orders.clone()).await.unwrap();
    assert_eq!(indexes[0].name, "orders_pkey");
    assert!(indexes[0].primary);

    // from another database, by name
    s.use_database("analytics").await.unwrap();
    let cols = s.columns(orders).await.unwrap();
    assert_eq!(cols[2].data_type, "numeric(12,2)");
    assert_eq!(
        s.current_database().await.unwrap().as_deref(),
        Some("analytics")
    );
});

it!(cancels_and_stays_usable, |s| {
    let handle = s.execute("SELECT pg_sleep(60)");
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    handle.cancel_token().cancel();
    let mut handle = handle;
    let mut last = None;
    while let Some(e) = handle.next().await {
        last = Some(e);
    }
    assert!(
        matches!(last, Some(QueryEvent::Finished(Err(DbError::Cancelled)))),
        "{last:?}"
    );
    let events = run(&s, "SELECT 7").await;
    assert_eq!(rows_of(&events), vec![vec![Value::Int(7)]]);
});

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_only_refuses_writes_on_the_server() {
    if !enabled() {
        return;
    }
    let mut conn = PostgresDriver
        .connect(&ConnectConfig {
            read_only: true,
            ..config()
        })
        .await
        .unwrap();
    let (tx, _rx) = tokio::sync::mpsc::channel(16);
    let mut sink = ResultSink::new(tx);
    let err = conn
        .execute("CREATE TABLE should_not_exist (x int)", &mut sink)
        .await
        .err()
        .unwrap();
    assert!(
        matches!(&err, DbError::Query { code: Some(c), .. } if c == "25006"),
        "{err:?}"
    );
}

fn cert(name: &str) -> Option<std::path::PathBuf> {
    Some(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../docker/tls")
            .join(name),
    )
}

fn with_tls(mode: TlsMode, ca: bool) -> ConnectConfig {
    ConnectConfig {
        tls: crate::TlsConfig {
            mode,
            ca_file: if ca { cert("ca.pem") } else { None },
            ..Default::default()
        },
        ..config()
    }
}

async fn try_connect(rt: &DbRuntime, config: ConnectConfig) -> crate::Result<Session> {
    rt.connect(Arc::new(PostgresDriver), config, SessionOptions::default())
        .await
}

async fn encrypted(s: &Session) -> bool {
    let events = run(
        s,
        "SELECT ssl FROM pg_stat_ssl WHERE pid = pg_backend_pid()",
    )
    .await;
    rows_of(&events) == vec![vec![Value::Bool(true)]]
}

it!(tls_require_and_disable, |_s| {
    let rt = DbRuntime::new().unwrap();
    for (mode, tls) in [
        (TlsMode::Require, true),
        (TlsMode::Prefer, true),
        (TlsMode::Disable, false),
    ] {
        let s = try_connect(&rt, with_tls(mode, false)).await.unwrap();
        assert_eq!(encrypted(&s).await, tls, "{mode:?}");
    }
});

it!(tls_verify_checks_the_ca_and_the_name, |_s| {
    let rt = DbRuntime::new().unwrap();
    let s = try_connect(&rt, with_tls(TlsMode::VerifyFull, true))
        .await
        .unwrap();
    assert!(encrypted(&s).await);

    // the test CA isn't trusted unless given
    let err = try_connect(&rt, with_tls(TlsMode::VerifyCa, false))
        .await
        .err()
        .unwrap();
    assert!(matches!(err, DbError::Tls(_)), "{err:?}");

    // the certificate names 127.0.0.1 but not localhost
    let by_name = ConnectConfig {
        host: "localhost".into(),
        ..with_tls(TlsMode::VerifyFull, true)
    };
    let err = try_connect(&rt, by_name.clone()).await.err().unwrap();
    assert!(matches!(err, DbError::Tls(_)), "{err:?}");
    let mut ca_only = by_name.clone();
    ca_only.tls.mode = TlsMode::VerifyCa;
    try_connect(&rt, ca_only).await.unwrap();
    // a tunnel checks the name the certificate was issued for
    let mut tunnelled = by_name;
    tunnelled.tls.server_name = Some("db.honcho.test".into());
    try_connect(&rt, tunnelled).await.unwrap();

    let mut missing = with_tls(TlsMode::VerifyCa, false);
    missing.tls.ca_file = cert("nope.pem");
    match try_connect(&rt, missing).await.err().unwrap() {
        DbError::Tls(m) => assert!(m.contains("nope.pem"), "{m}"),
        other => panic!("{other:?}"),
    }
});

it!(tls_client_certificate, |_s| {
    let rt = DbRuntime::new().unwrap();
    let x509 = |with_cert: bool| {
        let mut c = ConnectConfig {
            user: "honcho_x509".into(),
            password: None,
            ..with_tls(TlsMode::VerifyFull, true)
        };
        if with_cert {
            c.tls.cert_file = cert("client-cert.pem");
            c.tls.key_file = cert("client-key.pem");
        }
        c
    };
    let s = try_connect(&rt, x509(true)).await.unwrap();
    let events = run(&s, "SELECT COUNT(*) FROM users").await;
    assert_eq!(rows_of(&events), vec![vec![Value::Int(2)]]);
    let err = try_connect(&rt, x509(false)).await.err().unwrap();
    assert!(matches!(err, DbError::Auth(_)), "{err:?}");
});

// Pull, edit and apply routines in their own schema, in `analytics`.
it!(project_round_trip, |s| {
    use crate::project::{self, Manifest, Project, Status};
    s.use_database("analytics").await.unwrap();
    let setup = run(
        &s,
        "CREATE SCHEMA IF NOT EXISTS proj;
         DROP VIEW IF EXISTS proj.base;
         DROP PROCEDURE IF EXISTS proj.double_it;
         CREATE PROCEDURE proj.double_it(p int, INOUT doubled int) LANGUAGE plpgsql AS $$
         BEGIN
             doubled := p * 2;
         END $$;
         CREATE VIEW proj.base AS SELECT 1 AS n",
    )
    .await;
    assert!(
        matches!(setup.last(), Some(QueryEvent::Finished(Ok(_)))),
        "{setup:?}"
    );
    let dir = tempfile::tempdir().unwrap();
    let p = Project::init(
        dir.path(),
        Manifest {
            connection: "docker".into(),
            database: "analytics".into(),
        },
    )
    .unwrap();
    let entries = project::status(&s, &p).await.unwrap();
    for e in &entries {
        project::pull(&p, DriverKind::Postgres, e).unwrap();
    }
    let entries = project::status(&s, &p).await.unwrap();
    for e in &entries {
        assert_eq!(e.status, Status::Same, "{}", e.object.name);
    }

    let file = dir.path().join("procedures/proj.double_it.sql");
    let text = std::fs::read_to_string(&file).unwrap();
    assert!(
        text.starts_with("CREATE OR REPLACE PROCEDURE proj.double_it"),
        "{text}"
    );
    std::fs::write(&file, text.replace("p * 2", "p * 3")).unwrap();
    let entries = project::status(&s, &p).await.unwrap();
    let edited = entries
        .iter()
        .find(|e| e.object.name == "double_it")
        .unwrap();
    assert_eq!(edited.status, Status::Edited);
    project::apply(&s, &p, DriverKind::Postgres, edited)
        .await
        .unwrap();
    let entries = project::status(&s, &p).await.unwrap();
    for e in &entries {
        assert_eq!(e.status, Status::Same, "{}", e.object.name);
    }

    // the call the Run form builds
    let object = ObjectRef::new(ObjectKind::Procedure, "double_it")
        .in_database("analytics")
        .in_schema("proj");
    let params = s.parameters(object.clone()).await.unwrap();
    let sql = crate::routine::call(
        DriverKind::Postgres,
        &object,
        &params,
        &["5".into(), "".into()],
    );
    let events = run(&s, &sql).await;
    assert_eq!(rows_of(&events), vec![vec![Value::Int(15)]], "{sql}");
});
