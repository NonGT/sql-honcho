//! Run against the servers from docker/compose.yml with `HONCHO_IT=1`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use super::*;
use crate::conformance::{Fixture, Sql};
use crate::{DbRuntime, QueryEvent, Session, SessionOptions, Value};

#[derive(Clone, Copy, PartialEq)]
enum Server {
    MySql,
    MariaDb,
}

fn enabled() -> bool {
    std::env::var_os("HONCHO_IT").is_some()
}

fn config(server: Server) -> ConnectConfig {
    ConnectConfig {
        host: "127.0.0.1".into(),
        port: Some(match server {
            Server::MySql => 33306,
            Server::MariaDb => 33307,
        }),
        user: "honcho".into(),
        password: Some("Honcho_test1".into()),
        database: Some("app".into()),
        ..Default::default()
    }
}

// digits cross-joined, since a recursive CTE stops at 1000 rows on MySQL
fn rows(n: usize) -> String {
    format!(
        "WITH d(x) AS (SELECT 0 UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 \
         UNION ALL SELECT 4 UNION ALL SELECT 5 UNION ALL SELECT 6 UNION ALL SELECT 7 \
         UNION ALL SELECT 8 UNION ALL SELECT 9) \
         SELECT a.x + 10 * b.x + 100 * c.x + 1000 * e.x + 10000 * f.x + 1 AS i \
         FROM d a, d b, d c, d e, d f ORDER BY i LIMIT {n}"
    )
}

fn fixture_for(server: Server) -> Fixture {
    Fixture {
        enabled: enabled(),
        driver: Arc::new(MySqlDriver),
        config: config(server),
        bad_auth: ConnectConfig {
            password: Some("wrong".into()),
            ..config(server)
        },
        seed_scope: Scope {
            database: Some("app".into()),
            schema: None,
        },
        can_switch_database: true,
        sql: Sql {
            types: (
                "SELECT 42 AS n, CAST(12.50 AS DECIMAL(12, 2)) AS d, 'สวัสดี 😀' AS t, \
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
            rows,
            two_results: "SELECT 1; SELECT 2".into(),
            rows_affected: (
                "CREATE TEMPORARY TABLE t (x INT); INSERT INTO t VALUES (1), (2), (3)".into(),
                3,
            ),
            syntax_error: "SELEC 1".into(),
            warning: Some("SELECT CAST('12abc' AS SIGNED)".into()),
            sleep: "SELECT SLEEP(60)".into(),
        },
    }
}

mod mysql8 {
    use super::*;
    crate::conformance_tests!(fixture_for(Server::MySql));
}

mod mariadb {
    use super::*;
    crate::conformance_tests!(fixture_for(Server::MariaDb));
}

async fn session(rt: &DbRuntime, server: Server) -> Session {
    rt.connect(
        Arc::new(MySqlDriver),
        config(server),
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

fn finished_ok(events: &[QueryEvent]) {
    assert!(
        matches!(events.last(), Some(QueryEvent::Finished(Ok(_)))),
        "{events:?}"
    );
}

macro_rules! both_servers {
    ($($name:ident),* $(,)?) => {
        $(
            mod $name {
                use super::*;

                #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
                async fn mysql() {
                    if enabled() {
                        super::$name(Server::MySql).await;
                    }
                }

                #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
                async fn mariadb() {
                    if enabled() {
                        super::$name(Server::MariaDb).await;
                    }
                }
            }
        )*
    };
}

both_servers!(
    maps_more_types,
    runs_delimiter_scripts_with_several_result_sets,
    reports_error_lines_in_the_script,
    cancels_sleep_within_a_second,
    marks_a_killed_connection_lost,
);

async fn maps_more_types(server: Server) {
    let rt = DbRuntime::new().unwrap();
    let s = session(&rt, server).await;
    let events = run(
        &s,
        "SET SESSION sql_mode = '';
         CREATE TEMPORARY TABLE odd (bits BIT(3), d DATE, g GEOMETRY, e ENUM('a', 'b'), b BLOB);
         INSERT INTO odd VALUES (b'101', '0000-00-00', ST_GeomFromText('POINT(1 2)'), 'b', x'00FF');
         SELECT CAST(18446744073709551615 AS UNSIGNED), 1.5e0, x'DEADBEEF', TIME '12:34:56',
                TIMESTAMP '2024-01-31 12:00:00', odd.* FROM odd",
    )
    .await;
    finished_ok(&events);
    let columns = events
        .iter()
        .find_map(|e| match e {
            QueryEvent::Columns(c) => Some(c),
            _ => None,
        })
        .unwrap();
    let types: Vec<&str> = columns.iter().map(|c| c.type_name.as_str()).collect();
    assert_eq!(
        types,
        [
            "bigint unsigned",
            "double",
            "varbinary",
            "time",
            "datetime",
            "bit",
            "date",
            "geometry",
            "enum",
            "blob"
        ]
    );
    assert_eq!(
        rows_of(&events),
        vec![vec![
            Value::UInt(u64::MAX),
            Value::Float(1.5),
            Value::Bytes(vec![0xde, 0xad, 0xbe, 0xef]),
            Value::Time("12:34:56".into()),
            Value::DateTime("2024-01-31 12:00:00".into()),
            Value::UInt(5),
            Value::Date("0000-00-00".into()),
            Value::Other("POINT(1 2)".into()),
            Value::Text("b".into()),
            Value::Bytes(vec![0, 0xff]),
        ]]
    );

    if server == Server::MySql {
        let events = run(&s, r#"SELECT CAST('{"a": 1}' AS JSON)"#).await;
        assert_eq!(
            rows_of(&events),
            vec![vec![Value::Json(r#"{"a": 1}"#.into())]]
        );
    }
}

async fn runs_delimiter_scripts_with_several_result_sets(server: Server) {
    let rt = DbRuntime::new().unwrap();
    let s = session(&rt, server).await;
    let events = run(
        &s,
        "DROP PROCEDURE IF EXISTS honcho_two_sets;
         DELIMITER //
         CREATE PROCEDURE honcho_two_sets()
         BEGIN
             SELECT 1 AS a;
             SELECT 2 AS b, 'x;y' AS c;
         END //
         DELIMITER ;
         CALL honcho_two_sets();
         DROP PROCEDURE honcho_two_sets;",
    )
    .await;
    finished_ok(&events);
    let columns = events
        .iter()
        .filter(|e| matches!(e, QueryEvent::Columns(_)))
        .count();
    assert_eq!(columns, 2, "{events:?}");
    assert_eq!(
        rows_of(&events),
        vec![
            vec![Value::Int(1)],
            vec![Value::Int(2), Value::Text("x;y".into())]
        ]
    );
}

async fn reports_error_lines_in_the_script(server: Server) {
    let rt = DbRuntime::new().unwrap();
    let s = session(&rt, server).await;
    let events = run(&s, "SELECT 1;\n\nSELECT 2,\n  3 FORM orders;\nSELECT 4").await;
    match events.last() {
        Some(QueryEvent::Finished(Err(DbError::Query { code, line, .. }))) => {
            assert_eq!(code.as_deref(), Some("1064"));
            assert_eq!(*line, Some(4));
        }
        other => panic!("expected a query error, got {other:?}"),
    }
    // the statement after the error never ran
    assert_eq!(rows_of(&events), vec![vec![Value::Int(1)]]);
}

async fn cancels_sleep_within_a_second(server: Server) {
    let rt = DbRuntime::new().unwrap();
    let s = session(&rt, server).await;
    let handle = s.execute("SELECT SLEEP(60); SELECT 'not reached'");
    let token = handle.cancel_token();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        token.cancel();
    });
    let started = Instant::now();
    let mut handle = handle;
    let mut events = Vec::new();
    while let Some(e) = handle.next().await {
        events.push(e);
    }
    let elapsed = started.elapsed();
    assert!(
        matches!(
            events.last(),
            Some(QueryEvent::Finished(Err(DbError::Cancelled)))
        ),
        "{events:?}"
    );
    assert!(elapsed < Duration::from_secs(1), "took {elapsed:?}");
    assert!(!rows_of(&events).contains(&vec![Value::Text("not reached".into())]));
    finished_ok(&run(&s, "SELECT 1").await);
}

async fn marks_a_killed_connection_lost(server: Server) {
    let rt = DbRuntime::new().unwrap();
    let s = session(&rt, server).await;
    let events = run(&s, "SELECT CONNECTION_ID()").await;
    let id = match &rows_of(&events)[0][0] {
        Value::Int(i) => *i as u64,
        Value::UInt(u) => *u,
        other => panic!("{other:?}"),
    };
    let other = session(&rt, server).await;
    finished_ok(&run(&other, &format!("KILL {id}")).await);

    let events = run(&s, "SELECT 1").await;
    match events.last() {
        Some(QueryEvent::Finished(Err(e))) => assert!(e.is_fatal(), "{e:?}"),
        other => panic!("expected the connection to be gone, got {other:?}"),
    }
}
