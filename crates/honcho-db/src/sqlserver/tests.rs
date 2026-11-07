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
        port: Some(31433),
        user: "honcho".into(),
        password: Some("Honcho_test1".into()),
        database: Some("app".into()),
        ..Default::default()
    }
}

fn sql_server() -> Fixture {
    Fixture {
        enabled: enabled(),
        driver: Arc::new(SqlServerDriver),
        config: config(),
        bad_auth: Some(ConnectConfig {
            password: Some("wrong".into()),
            ..config()
        }),
        databases: &["app", "analytics"],
        routines: true,
        seed_scope: Scope {
            database: Some("app".into()),
            schema: Some("dbo".into()),
        },
        can_switch_database: true,
        sql: Sql {
            types: (
                "SELECT 42 AS n, CAST(12.50 AS DECIMAL(12, 2)) AS d, N'สวัสดี 😀' AS t, \
                 NULL AS z, CAST('2024-01-31' AS date) AS day"
                    .into(),
                vec![
                    Value::Int(42),
                    Value::Decimal("12.50".into()),
                    Value::Text("สวัสดี 😀".into()),
                    Value::Null,
                    Value::Date("2024-01-31".into()),
                ],
            ),
            rows: |n| {
                format!(
                    "SELECT TOP ({n}) ROW_NUMBER() OVER (ORDER BY (SELECT NULL)) AS i \
                     FROM sys.all_objects a CROSS JOIN sys.all_objects b ORDER BY i"
                )
            },
            two_results: "SELECT 1; SELECT 2".into(),
            rows_affected: (
                "CREATE TABLE #t (x INT); INSERT INTO #t VALUES (1), (2), (3)".into(),
                3,
            ),
            syntax_error: "SELEC 1".into(),
            warning: Some("PRINT 'hello'".into()),
            sleep: "WAITFOR DELAY '00:01:00'".into(),
        },
    }
}

crate::conformance_tests!(sql_server());

async fn session(rt: &DbRuntime) -> Session {
    rt.connect(
        Arc::new(SqlServerDriver),
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

it!(decodes_sql_server_types, |s| {
    let events = run(
        &s,
        "SELECT CAST('6F9619FF-8B86-D011-B42D-00C04FC964FF' AS uniqueidentifier), \
         CAST('2024-01-31 10:20:30.1234567 +07:00' AS datetimeoffset(7)), \
         CAST(12.5 AS money), CAST('<a>1</a>' AS xml), CAST(7 AS sql_variant), \
         CAST('2024-01-31 10:20:30.5' AS datetime), CAST('10:20:30.12' AS time(2)), \
         CAST(1 AS bit), 0x0A0B",
    )
    .await;
    assert!(
        matches!(events.last(), Some(QueryEvent::Finished(Ok(_)))),
        "{events:?}"
    );
    assert_eq!(
        rows_of(&events),
        vec![vec![
            Value::Other("6F9619FF-8B86-D011-B42D-00C04FC964FF".into()),
            Value::DateTime("2024-01-31 10:20:30.1234567 +07:00".into()),
            Value::Decimal("12.5000".into()),
            Value::Text("<a>1</a>".into()),
            Value::Int(7),
            Value::DateTime("2024-01-31 10:20:30.500".into()),
            Value::Time("10:20:30.12".into()),
            Value::Bool(true),
            Value::Bytes(vec![10, 11]),
        ]]
    );
});

it!(reports_print_and_raiserror_in_order, |s| {
    let events = run(
        &s,
        "PRINT 'one'; SELECT 1; RAISERROR('two', 10, 1); PRINT N'สาม'",
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
            (MessageLevel::Info, "two".to_string()),
            (MessageLevel::Info, "สาม".to_string()),
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

it!(runs_go_batches_and_points_errors_at_script_lines, |s| {
    let events = run(&s, "SELECT 1\nGO 2\nSELECT 2\nSELEC 3\nGO\nSELECT 4").await;
    // both runs of the first batch, then nothing after the failed one
    assert_eq!(
        rows_of(&events),
        vec![vec![Value::Int(1)], vec![Value::Int(1)]]
    );
    match events.last() {
        Some(QueryEvent::Finished(Err(DbError::Query { line, code, .. }))) => {
            assert_eq!(*line, Some(4));
            assert_eq!(code.as_deref(), Some("102"));
        }
        other => panic!("{other:?}"),
    }
});

it!(keeps_going_after_a_failed_statement, |s| {
    let events = run(&s, "SELECT 1/0;\nSELECT 'after';\nSELECT CAST('x' AS int)").await;
    assert_eq!(rows_of(&events), vec![vec![Value::Text("after".into())]]);
    let errors: Vec<_> = messages(&events)
        .into_iter()
        .filter(|(level, _)| *level == MessageLevel::Error)
        .collect();
    assert_eq!(errors.len(), 1, "{events:?}");
    match events.last() {
        Some(QueryEvent::Finished(Err(DbError::Query { line, message, .. }))) => {
            assert_eq!(*line, Some(1));
            assert!(message.contains("zero"), "{message}");
        }
        other => panic!("{other:?}"),
    }
});

it!(lists_schemas_and_scripts_tables, |s| {
    let schemas = s.schemas("app").await.unwrap();
    assert!(schemas.iter().any(|x| x == "dbo"), "{schemas:?}");
    assert!(
        !schemas.iter().any(|x| x == "db_owner" || x == "sys"),
        "{schemas:?}"
    );

    let orders = ObjectRef::new(ObjectKind::Table, "orders")
        .in_database("app")
        .in_schema("dbo");
    let def = s.definition(orders.clone()).await.unwrap();
    assert!(def.starts_with("CREATE TABLE [dbo].[orders]"), "{def}");
    assert!(def.contains("[amount] decimal(12, 2) NOT NULL"), "{def}");
    assert!(def.contains("IDENTITY(1, 1)"), "{def}");
    assert!(def.contains("PRIMARY KEY ([id])"), "{def}");

    // from another database, by name
    s.use_database("analytics").await.unwrap();
    let cols = s.columns(orders).await.unwrap();
    assert_eq!(cols[2].data_type, "decimal(12, 2)");
});

it!(knows_when_a_transaction_is_open, |s| {
    assert!(!s.in_transaction().await.unwrap());
    run(&s, "BEGIN TRAN; SELECT 1").await;
    assert!(s.in_transaction().await.unwrap());
    run(&s, "COMMIT").await;
    assert!(!s.in_transaction().await.unwrap());
});

it!(cancels_and_stays_usable_for_metadata, |s| {
    let handle = s.execute("WAITFOR DELAY '00:01:00'");
    handle.cancel_token().cancel();
    let mut handle = handle;
    while handle.next().await.is_some() {}
    assert_eq!(s.current_database().await.unwrap().as_deref(), Some("app"));
});

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tls_modes() {
    if !enabled() {
        return;
    }
    let with = |mode| ConnectConfig {
        tls: crate::TlsConfig {
            mode,
            ..Default::default()
        },
        ..config()
    };
    for mode in [TlsMode::Disable, TlsMode::Prefer, TlsMode::Require] {
        let mut conn = SqlServerDriver.connect(&with(mode)).await.unwrap();
        conn.ping().await.unwrap();
    }
    // the container's certificate is self-signed
    let err = SqlServerDriver
        .connect(&with(TlsMode::VerifyFull))
        .await
        .err()
        .unwrap();
    assert!(matches!(err, DbError::Tls(_)), "{err:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_only_sees_statements_without_semicolons() {
    if !enabled() {
        return;
    }
    let rt = DbRuntime::new().unwrap();
    let s = rt
        .connect(
            Arc::new(SqlServerDriver),
            ConnectConfig {
                read_only: true,
                ..config()
            },
            SessionOptions::default(),
        )
        .await
        .unwrap();
    let events = run(
        &s,
        "SELECT COUNT(*) FROM users\nGO\nSELECT 1 DELETE FROM orders",
    )
    .await;
    assert!(
        matches!(
            events.last(),
            Some(QueryEvent::Finished(Err(DbError::ReadOnly { line: 3, .. })))
        ),
        "{events:?}"
    );
    assert_eq!(rows_of(&events), Vec::<Vec<Value>>::new());
    let events = run(
        &s,
        "WITH x AS (SELECT id FROM users) SELECT * FROM x WITH (NOLOCK)",
    )
    .await;
    assert!(
        matches!(events.last(), Some(QueryEvent::Finished(Ok(_)))),
        "{events:?}"
    );
}

// Pull, edit and apply routines in their own schema, in `analytics`,
// which no other test puts routines in.
it!(project_round_trip, |s| {
    use crate::project::{self, Manifest, Project, Status};
    let setup = run(
        &s,
        "USE analytics
         GO
         IF SCHEMA_ID('proj') IS NULL EXEC('CREATE SCHEMA proj')
         DROP VIEW IF EXISTS proj.base
         DROP PROCEDURE IF EXISTS proj.double_it
         GO
         -- doubles; really
         CREATE PROC proj.double_it @p int, @doubled int OUTPUT AS
         SET @doubled = @p * 2;
         GO
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
        project::pull(&p, DriverKind::SqlServer, e).unwrap();
    }
    let entries = project::status(&s, &p).await.unwrap();
    for e in &entries {
        assert_eq!(e.status, Status::Same, "{}", e.object.name);
    }

    let file = dir.path().join("procedures/proj.double_it.sql");
    let text = std::fs::read_to_string(&file).unwrap();
    assert!(text.starts_with("-- doubles; really\n"), "{text}");
    assert!(
        text.contains("CREATE OR ALTER PROC proj.double_it"),
        "{text}"
    );
    std::fs::write(&file, text.replace("@p * 2", "@p * 3")).unwrap();
    let entries = project::status(&s, &p).await.unwrap();
    let edited = entries
        .iter()
        .find(|e| e.object.name == "double_it")
        .unwrap();
    assert_eq!(edited.status, Status::Edited);
    project::apply(&s, &p, DriverKind::SqlServer, edited)
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
        DriverKind::SqlServer,
        &object,
        &params,
        &["5".into(), "".into()],
    );
    let events = run(&s, &sql).await;
    assert_eq!(rows_of(&events), vec![vec![Value::Int(15)]], "{sql}");
});
