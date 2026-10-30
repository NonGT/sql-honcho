//! Tests every driver has to pass, written once against the session API.
//!
//! A driver supplies a [`Fixture`] (how to connect, plus the SQL its dialect
//! needs for each case) and expands [`conformance_tests!`](crate::conformance_tests)
//! in its test code. The database must hold the shared seed from
//! `docker/seed`: databases `app` and `analytics`; in `app` the tables
//! `users` and `orders`, the view `active_users`, the procedure
//! `refresh_stats` and the function `order_total`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::types::{ConnectConfig, ObjectKind, QueryEvent, Scope};
use crate::{DbError, DbRuntime, Driver, QueryHandle, Session, SessionOptions, Value};

pub struct Fixture {
    /// False skips every check. Real drivers set it from the `HONCHO_IT`
    /// environment variable, so plain `cargo test` stays offline.
    pub enabled: bool,
    pub driver: Arc<dyn Driver>,
    pub config: ConnectConfig,
    pub bad_auth: ConnectConfig,
    /// Where the seeded objects live (database, and schema where the
    /// server has one).
    pub seed_scope: Scope,
    /// Whether `use_database` can move the connection to another database.
    pub can_switch_database: bool,
    pub sql: Sql,
}

pub struct Sql {
    /// One row, and the values it must decode to. Should include NULL,
    /// non-ASCII text and a decimal.
    pub types: (String, Vec<Value>),
    /// N rows whose first column is an integer counting up by one.
    pub rows: fn(usize) -> String,
    /// Exactly two result sets in one call.
    pub two_results: String,
    /// Statements where the last one affects this many rows.
    pub rows_affected: (String, u64),
    pub syntax_error: String,
    /// A statement that produces a server warning, if the server has them.
    pub warning: Option<String>,
    /// Runs for at least 30 seconds unless cancelled.
    pub sleep: String,
}

/// Return early when the fixture is disabled.
macro_rules! check {
    ($f:ident) => {
        if !$f.enabled {
            eprintln!("skipped: test database not enabled");
            return;
        }
    };
}

async fn connect(f: &Fixture, rt: &DbRuntime) -> Session {
    rt.connect(
        f.driver.clone(),
        f.config.clone(),
        SessionOptions::default(),
    )
    .await
    .unwrap_or_else(|e| panic!("connect failed: {e}"))
}

async fn collect(mut handle: QueryHandle) -> Vec<QueryEvent> {
    let mut out = Vec::new();
    while let Some(e) = handle.next().await {
        out.push(e);
    }
    out
}

fn finished(events: &[QueryEvent]) -> &Result<crate::QueryStats, DbError> {
    match events.last() {
        Some(QueryEvent::Finished(r)) => r,
        other => panic!("last event isn't Finished: {other:?}"),
    }
}

fn result_sets(events: &[QueryEvent]) -> Vec<Vec<Vec<Value>>> {
    let mut sets: Vec<Vec<Vec<Value>>> = Vec::new();
    for e in events {
        match e {
            QueryEvent::Columns(_) => sets.push(Vec::new()),
            QueryEvent::Rows(rows) => sets
                .last_mut()
                .expect("rows before columns")
                .extend(rows.iter().cloned()),
            _ => {}
        }
    }
    sets
}

async fn assert_usable(s: &Session, f: &Fixture) {
    let events = collect(s.execute(f.sql.types.0.clone())).await;
    assert!(finished(&events).is_ok(), "session unusable: {events:?}");
}

pub async fn connects_and_pings(f: Fixture) {
    check!(f);
    let rt = DbRuntime::new().unwrap();
    let s = connect(&f, &rt).await;
    s.ping().await.unwrap();
    s.close().await;
}

pub async fn rejects_bad_credentials(f: Fixture) {
    check!(f);
    let rt = DbRuntime::new().unwrap();
    let err = rt
        .connect(
            f.driver.clone(),
            f.bad_auth.clone(),
            SessionOptions::default(),
        )
        .await
        .err()
        .expect("connected with bad credentials");
    assert!(
        matches!(err, DbError::Auth(_)),
        "expected an auth error, got {err:?}"
    );
}

pub async fn decodes_types(f: Fixture) {
    check!(f);
    let rt = DbRuntime::new().unwrap();
    let s = connect(&f, &rt).await;
    let (sql, expected) = &f.sql.types;
    let events = collect(s.execute(sql.clone())).await;
    finished(&events).as_ref().unwrap();
    let sets = result_sets(&events);
    assert_eq!(sets.len(), 1, "{events:?}");
    assert_eq!(sets[0], vec![expected.clone()]);
}

pub async fn streams_many_rows(f: Fixture) {
    check!(f);
    let n = 10_000;
    let rt = DbRuntime::new().unwrap();
    let s = connect(&f, &rt).await;
    let events = collect(s.execute((f.sql.rows)(n))).await;
    finished(&events).as_ref().unwrap();
    let sets = result_sets(&events);
    assert_eq!(sets.len(), 1);
    let ids: Vec<i64> = sets[0]
        .iter()
        .map(|row| match &row[0] {
            Value::Int(i) => *i,
            Value::UInt(u) => *u as i64,
            other => panic!("first column isn't an integer: {other:?}"),
        })
        .collect();
    assert_eq!(ids.len(), n);
    assert!(
        ids.windows(2).all(|w| w[1] == w[0] + 1),
        "rows out of order"
    );
    let batches = events
        .iter()
        .filter(|e| matches!(e, QueryEvent::Rows(_)))
        .count();
    assert!(batches > 1, "expected rows to arrive in several batches");
}

pub async fn returns_multiple_result_sets(f: Fixture) {
    check!(f);
    let rt = DbRuntime::new().unwrap();
    let s = connect(&f, &rt).await;
    let events = collect(s.execute(f.sql.two_results.clone())).await;
    finished(&events).as_ref().unwrap();
    assert_eq!(result_sets(&events).len(), 2, "{events:?}");
}

pub async fn reports_rows_affected(f: Fixture) {
    check!(f);
    let rt = DbRuntime::new().unwrap();
    let s = connect(&f, &rt).await;
    let (sql, expected) = &f.sql.rows_affected;
    let events = collect(s.execute(sql.clone())).await;
    finished(&events).as_ref().unwrap();
    let last = events.iter().rev().find_map(|e| match e {
        QueryEvent::Done { rows_affected } => Some(*rows_affected),
        _ => None,
    });
    assert_eq!(last, Some(Some(*expected)), "{events:?}");
}

pub async fn reports_query_errors(f: Fixture) {
    check!(f);
    let rt = DbRuntime::new().unwrap();
    let s = connect(&f, &rt).await;
    let events = collect(s.execute(f.sql.syntax_error.clone())).await;
    match finished(&events) {
        Err(DbError::Query { message, .. }) => assert!(!message.is_empty()),
        other => panic!("expected a query error, got {other:?}"),
    }
    assert_usable(&s, &f).await;
}

pub async fn reports_warnings(f: Fixture) {
    check!(f);
    let Some(sql) = f.sql.warning.clone() else {
        eprintln!("skipped: driver has no warnings");
        return;
    };
    let rt = DbRuntime::new().unwrap();
    let s = connect(&f, &rt).await;
    let events = collect(s.execute(sql)).await;
    finished(&events).as_ref().unwrap();
    assert!(
        events.iter().any(|e| matches!(e, QueryEvent::Message(_))),
        "no message in {events:?}"
    );
}

pub async fn cancels_long_queries(f: Fixture) {
    check!(f);
    let rt = DbRuntime::new().unwrap();
    let s = connect(&f, &rt).await;
    let handle = s.execute(f.sql.sleep.clone());
    let token = handle.cancel_token();
    let started = Instant::now();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        token.cancel();
    });
    let events = collect(handle).await;
    assert_eq!(finished(&events), &Err(DbError::Cancelled));
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "cancel took {:?}",
        started.elapsed()
    );
    assert_usable(&s, &f).await;
}

pub async fn lists_metadata(f: Fixture) {
    check!(f);
    let rt = DbRuntime::new().unwrap();
    let s = connect(&f, &rt).await;

    let dbs = s.databases().await.unwrap();
    for db in ["app", "analytics"] {
        assert!(dbs.iter().any(|d| d == db), "{db} missing from {dbs:?}");
    }

    let objects = s.objects(f.seed_scope.clone(), vec![]).await.unwrap();
    let find = |kind: ObjectKind, name: &str| {
        objects
            .iter()
            .find(|o| o.kind == kind && o.name.eq_ignore_ascii_case(name))
            .cloned()
            .unwrap_or_else(|| panic!("{kind:?} {name} missing from {objects:?}"))
    };
    let orders = find(ObjectKind::Table, "orders");
    find(ObjectKind::Table, "users");
    find(ObjectKind::View, "active_users");
    let procedure = find(ObjectKind::Procedure, "refresh_stats");
    find(ObjectKind::Function, "order_total");

    let routines = s
        .objects(
            f.seed_scope.clone(),
            vec![ObjectKind::Procedure, ObjectKind::Function],
        )
        .await
        .unwrap();
    assert!(routines.iter().all(|o| o.kind.is_routine()), "{routines:?}");

    let cols = s.columns(orders).await.unwrap();
    let id = cols
        .iter()
        .find(|c| c.name.eq_ignore_ascii_case("id"))
        .expect("orders.id");
    assert!(id.primary_key);
    assert!(cols.iter().any(|c| c.name.eq_ignore_ascii_case("amount")));

    let def = s.definition(procedure).await.unwrap();
    assert!(def.to_lowercase().contains("refresh_stats"), "{def}");
}

pub async fn switches_database(f: Fixture) {
    check!(f);
    if !f.can_switch_database {
        eprintln!("skipped: driver can't switch databases");
        return;
    }
    let rt = DbRuntime::new().unwrap();
    let s = connect(&f, &rt).await;
    s.use_database("analytics").await.unwrap();
    assert_eq!(
        s.current_database().await.unwrap().as_deref(),
        Some("analytics")
    );
    assert!(s.use_database("no_such_database").await.is_err());
}

/// Expands to one `#[tokio::test]` per check, inside `mod conformance`.
/// The argument is an expression producing a [`Fixture`]; it is evaluated
/// once per test. Needs `tokio` with the `macros` feature.
#[macro_export]
macro_rules! conformance_tests {
    ($fixture:expr) => {
        mod conformance {
            #[allow(unused_imports)]
            use super::*;

            fn fixture() -> $crate::conformance::Fixture {
                $fixture
            }

            $crate::conformance_tests!(@tests
                connects_and_pings,
                rejects_bad_credentials,
                decodes_types,
                streams_many_rows,
                returns_multiple_result_sets,
                reports_rows_affected,
                reports_query_errors,
                reports_warnings,
                cancels_long_queries,
                lists_metadata,
                switches_database,
            );
        }
    };
    (@tests $($name:ident),* $(,)?) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
            async fn $name() {
                $crate::conformance::$name(fixture()).await;
            }
        )*
    };
}

#[cfg(test)]
mod fake {
    use std::sync::Arc;

    use crate::fake::{types_row, FakeDriver};
    use crate::types::{ConnectConfig, Scope};

    use super::{Fixture, Sql};

    crate::conformance_tests!(Fixture {
        enabled: true,
        driver: Arc::new(FakeDriver::default()),
        config: ConnectConfig {
            user: "dev".into(),
            database: Some("app".into()),
            ..Default::default()
        },
        bad_auth: ConnectConfig {
            user: "bad".into(),
            ..Default::default()
        },
        seed_scope: Scope {
            database: Some("app".into()),
            schema: None,
        },
        can_switch_database: true,
        sql: Sql {
            types: ("types".into(), types_row()),
            rows: |n| format!("rows {n}"),
            two_results: "rows 1; rows 2".into(),
            rows_affected: ("rows 1; update 3".into(), 3),
            syntax_error: "error syntax error near 'FORM'".into(),
            warning: Some("warn truncated".into()),
            sleep: "sleep 60000".into(),
        },
    });
}
