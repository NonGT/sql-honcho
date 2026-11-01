//! Run against the servers from docker/compose.yml with `HONCHO_IT=1`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use super::*;
use crate::conformance::{Fixture, Sql};
use crate::{DbRuntime, QueryEvent, Session, SessionOptions, TlsConfig, TlsMode, Value};

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
    read_only_is_enforced_by_the_server_too,
    reads_thai_from_a_table,
    tls_require_and_disable,
    tls_verify_checks_the_ca_and_the_name,
    tls_client_certificate,
);

fn cert(name: &str) -> Option<std::path::PathBuf> {
    Some(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../docker/tls")
            .join(name),
    )
}

fn with_tls(server: Server, mode: TlsMode, ca: bool) -> ConnectConfig {
    ConnectConfig {
        tls: TlsConfig {
            mode,
            ca_file: if ca { cert("ca.pem") } else { None },
            ..Default::default()
        },
        ..config(server)
    }
}

async fn try_connect(rt: &DbRuntime, config: ConnectConfig) -> Result<Session> {
    rt.connect(Arc::new(MySqlDriver), config, SessionOptions::default())
        .await
}

/// The cipher in use, empty on a plain connection.
async fn cipher(s: &Session) -> String {
    let events = run(s, "SHOW SESSION STATUS LIKE 'Ssl_cipher'").await;
    match &rows_of(&events)[..] {
        [row] => row[1].display(100).into_owned(),
        other => panic!("{other:?}"),
    }
}

async fn tls_require_and_disable(server: Server) {
    let rt = DbRuntime::new().unwrap();
    let s = try_connect(&rt, with_tls(server, TlsMode::Require, false))
        .await
        .unwrap();
    assert!(!cipher(&s).await.is_empty());
    let s = try_connect(&rt, with_tls(server, TlsMode::Prefer, false))
        .await
        .unwrap();
    assert!(!cipher(&s).await.is_empty());
    let s = try_connect(&rt, with_tls(server, TlsMode::Disable, false))
        .await
        .unwrap();
    assert_eq!(cipher(&s).await, "");
}

async fn tls_verify_checks_the_ca_and_the_name(server: Server) {
    let rt = DbRuntime::new().unwrap();
    let s = try_connect(&rt, with_tls(server, TlsMode::VerifyFull, true))
        .await
        .unwrap();
    assert!(!cipher(&s).await.is_empty());

    // the test CA isn't trusted unless given
    let err = try_connect(&rt, with_tls(server, TlsMode::VerifyCa, false))
        .await
        .err()
        .unwrap();
    assert!(matches!(err, DbError::Tls(_)), "{err:?}");

    // the certificate names 127.0.0.1 but not localhost
    let by_name = ConnectConfig {
        host: "localhost".into(),
        ..with_tls(server, TlsMode::VerifyFull, true)
    };
    let err = try_connect(&rt, by_name.clone()).await.err().unwrap();
    assert!(matches!(err, DbError::Tls(_)), "{err:?}");
    // verify-ca doesn't look at the name
    let ca_only = ConnectConfig {
        tls: TlsConfig {
            mode: TlsMode::VerifyCa,
            ..by_name.tls.clone()
        },
        ..by_name.clone()
    };
    try_connect(&rt, ca_only).await.unwrap();
    // a tunnel checks the name the certificate was issued for
    let mut tunnelled = by_name;
    tunnelled.tls.server_name = Some("db.honcho.test".into());
    try_connect(&rt, tunnelled).await.unwrap();

    let missing = ConnectConfig {
        tls: TlsConfig {
            mode: TlsMode::VerifyCa,
            ca_file: cert("nope.pem"),
            ..Default::default()
        },
        ..config(server)
    };
    match try_connect(&rt, missing).await.err().unwrap() {
        DbError::Tls(m) => assert!(m.contains("nope.pem"), "{m}"),
        other => panic!("{other:?}"),
    }
}

async fn tls_client_certificate(server: Server) {
    let rt = DbRuntime::new().unwrap();
    let x509 = |with_cert: bool| ConnectConfig {
        user: "honcho_x509".into(),
        tls: TlsConfig {
            mode: TlsMode::VerifyFull,
            ca_file: cert("ca.pem"),
            cert_file: if with_cert {
                cert("client-cert.pem")
            } else {
                None
            },
            key_file: if with_cert {
                cert("client-key.pem")
            } else {
                None
            },
            server_name: None,
        },
        ..config(server)
    };
    let s = try_connect(&rt, x509(true)).await.unwrap();
    finished_ok(&run(&s, "SELECT COUNT(*) FROM users").await);
    let err = try_connect(&rt, x509(false)).await.err().unwrap();
    assert!(matches!(err, DbError::Auth(_)), "{err:?}");
}

async fn reads_thai_from_a_table(server: Server) {
    let rt = DbRuntime::new().unwrap();
    let s = session(&rt, server).await;
    let events = run(&s, "SELECT email FROM users WHERE id = 2").await;
    assert_eq!(
        rows_of(&events),
        vec![vec![Value::Text("ข@example.com".into())]]
    );
}

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

async fn read_only_is_enforced_by_the_server_too(server: Server) {
    let config = ConnectConfig {
        read_only: true,
        ..config(server)
    };
    let rt = DbRuntime::new().unwrap();
    let s = rt
        .connect(
            Arc::new(MySqlDriver),
            config.clone(),
            SessionOptions::default(),
        )
        .await
        .unwrap();
    let events = run(&s, "SELECT COUNT(*) FROM users;\nDELETE FROM orders").await;
    assert!(
        matches!(
            events.last(),
            Some(QueryEvent::Finished(Err(DbError::ReadOnly { line: 2, .. })))
        ),
        "{events:?}"
    );
    assert_eq!(rows_of(&events), Vec::<Vec<Value>>::new());

    // past the session check, straight to the connection
    let mut conn = MySqlDriver.connect(&config).await.unwrap();
    let (tx, _rx) = tokio::sync::mpsc::channel(16);
    let mut sink = ResultSink::new(tx);
    match conn.execute("DELETE FROM orders", &mut sink).await {
        Err(DbError::Query { code, .. }) => assert_eq!(code.as_deref(), Some("1792")),
        other => panic!("expected the server to refuse, got {other:?}"),
    }
}
