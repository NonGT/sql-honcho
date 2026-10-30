use std::sync::Arc;
use std::time::{Duration, Instant};

use super::*;
use crate::fake::FakeDriver;
use crate::types::{MessageLevel, QueryEvent};

fn config(user: &str) -> ConnectConfig {
    ConnectConfig {
        user: user.into(),
        database: Some("app".into()),
        ..Default::default()
    }
}

async fn session(rt: &DbRuntime) -> Session {
    session_with(rt, SessionOptions::default()).await
}

async fn session_with(rt: &DbRuntime, options: SessionOptions) -> Session {
    rt.connect(Arc::new(FakeDriver::default()), config("dev"), options)
        .await
        .unwrap()
}

fn rows_of(events: &[QueryEvent]) -> Vec<i64> {
    events
        .iter()
        .filter_map(|e| match e {
            QueryEvent::Rows(r) => Some(r),
            _ => None,
        })
        .flatten()
        .map(|row| match row[0] {
            crate::Value::Int(i) => i,
            ref other => panic!("unexpected {other:?}"),
        })
        .collect()
}

async fn collect(mut handle: QueryHandle) -> Vec<QueryEvent> {
    let mut out = Vec::new();
    while let Some(e) = handle.next().await {
        out.push(e);
    }
    out
}

fn finished(events: &[QueryEvent]) -> &Result<QueryStats> {
    match events.last() {
        Some(QueryEvent::Finished(r)) => r,
        other => panic!("last event isn't Finished: {other:?}"),
    }
}

#[tokio::test]
async fn streams_rows_in_batches() {
    let rt = DbRuntime::new().unwrap();
    let s = session(&rt).await;
    let events = collect(s.execute("rows 10000")).await;

    assert!(matches!(&events[0], QueryEvent::Columns(c) if c.len() == 2));
    let batches: Vec<usize> = events
        .iter()
        .filter_map(|e| match e {
            QueryEvent::Rows(r) => Some(r.len()),
            _ => None,
        })
        .collect();
    assert_eq!(batches.iter().sum::<usize>(), 10000);
    assert_eq!(batches[0], 100, "first batch is small");
    assert!(
        batches.len() < 20,
        "rows are batched, got {} batches",
        batches.len()
    );
    assert!(matches!(
        events[events.len() - 2],
        QueryEvent::Done {
            rows_affected: None
        }
    ));
    assert!(finished(&events).is_ok());
}

#[tokio::test]
async fn multiple_result_sets_and_messages_keep_order() {
    let rt = DbRuntime::new().unwrap();
    let s = session(&rt).await;
    let events = collect(s.execute("rows 2; warn careful; update 3; rows 1")).await;
    let kinds: Vec<&str> = events
        .iter()
        .map(|e| match e {
            QueryEvent::Columns(_) => "columns",
            QueryEvent::Rows(_) => "rows",
            QueryEvent::Done { .. } => "done",
            QueryEvent::Message(_) => "message",
            QueryEvent::Finished(_) => "finished",
        })
        .collect();
    assert_eq!(
        kinds,
        ["columns", "rows", "done", "message", "done", "columns", "rows", "done", "finished"]
    );
    assert!(
        matches!(&events[3], QueryEvent::Message(m) if m.level == MessageLevel::Warning && m.text == "careful")
    );
    assert!(matches!(
        events[4],
        QueryEvent::Done {
            rows_affected: Some(3)
        }
    ));
}

#[tokio::test]
async fn query_errors_finish_the_run_and_keep_the_session() {
    let rt = DbRuntime::new().unwrap();
    let s = session(&rt).await;
    let events = collect(s.execute("rows 1; error syntax error near FROM; rows 1")).await;
    assert_eq!(
        finished(&events),
        &Err(DbError::query("syntax error near FROM"))
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, QueryEvent::Columns(_)))
            .count(),
        1
    );

    let events = collect(s.execute("rows 1")).await;
    assert!(finished(&events).is_ok());
}

#[tokio::test]
async fn cancel_stops_a_running_query() {
    let rt = DbRuntime::new().unwrap();
    let s = session(&rt).await;
    let handle = s.execute("sleep 30000");
    let token = handle.cancel_token();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        token.cancel();
    });
    let started = Instant::now();
    let events = collect(handle).await;
    assert_eq!(finished(&events), &Err(DbError::Cancelled));
    assert!(started.elapsed() < Duration::from_secs(2));

    // still usable afterwards
    assert!(finished(&collect(s.execute("rows 1")).await).is_ok());
}

#[tokio::test]
async fn a_query_that_ignores_cancel_breaks_the_session() {
    let rt = DbRuntime::new().unwrap();
    let s = session_with(
        &rt,
        SessionOptions {
            cancel_grace: Duration::from_millis(100),
        },
    )
    .await;
    let handle = s.execute("hang 30000");
    let token = handle.cancel_token();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        token.cancel();
    });
    let events = collect(handle).await;
    assert_eq!(finished(&events), &Err(DbError::Cancelled));

    let events = collect(s.execute("rows 1")).await;
    assert!(matches!(finished(&events), Err(DbError::ConnectionLost(_))));
    assert!(matches!(
        s.databases().await,
        Err(DbError::ConnectionLost(_))
    ));
}

#[tokio::test]
async fn lost_connection_is_reported_and_sticks() {
    let rt = DbRuntime::new().unwrap();
    let s = session(&rt).await;
    let events = collect(s.execute("lose")).await;
    assert!(matches!(finished(&events), Err(DbError::ConnectionLost(_))));
    assert!(s.ping().await.is_err());
}

#[tokio::test]
async fn dropping_the_handle_cancels() {
    let rt = DbRuntime::new().unwrap();
    let s = session(&rt).await;
    let started = Instant::now();
    drop(s.execute("sleep 30000"));
    // the next query only runs once the dropped one has stopped
    assert!(finished(&collect(s.execute("rows 1")).await).is_ok());
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn queued_queries_run_in_order() {
    let rt = DbRuntime::new().unwrap();
    let s = session(&rt).await;
    let a = s.execute("sleep 50; update 1");
    let b = s.execute("update 2");
    let (ea, eb) = tokio::join!(collect(a), collect(b));
    assert!(matches!(
        ea[0],
        QueryEvent::Done {
            rows_affected: Some(1)
        }
    ));
    assert!(matches!(
        eb[0],
        QueryEvent::Done {
            rows_affected: Some(2)
        }
    ));
}

#[tokio::test]
async fn metadata_calls() {
    let rt = DbRuntime::new().unwrap();
    let s = session(&rt).await;
    assert_eq!(s.databases().await.unwrap(), ["app", "analytics"]);
    assert_eq!(s.current_database().await.unwrap().as_deref(), Some("app"));

    let routines = s
        .objects(
            Scope::default(),
            vec![ObjectKind::Procedure, ObjectKind::Function],
        )
        .await
        .unwrap();
    assert_eq!(routines.len(), 2);
    assert!(routines.iter().all(|o| o.kind.is_routine()));

    let def = s.definition(routines[0].clone()).await.unwrap();
    assert!(def.starts_with("CREATE PROCEDURE"));

    s.use_database("analytics").await.unwrap();
    assert_eq!(
        s.current_database().await.unwrap().as_deref(),
        Some("analytics")
    );
    assert!(s.use_database("nope").await.is_err());

    let cols = s
        .columns(ObjectRef::new(ObjectKind::Table, "orders"))
        .await
        .unwrap();
    assert_eq!(cols.iter().filter(|c| c.primary_key).count(), 1);
}

#[tokio::test]
async fn auth_failure_and_closed_session() {
    let rt = DbRuntime::new().unwrap();
    let err = rt
        .connect(
            Arc::new(FakeDriver::default()),
            config("bad"),
            SessionOptions::default(),
        )
        .await
        .err()
        .unwrap();
    assert!(matches!(err, DbError::Auth(_)));

    let s = session(&rt).await;
    s.close().await;
    assert!(matches!(
        finished(&collect(s.execute("rows 1")).await),
        Err(DbError::ConnectionLost(_))
    ));
    assert!(s.ping().await.is_err());
}

#[tokio::test]
async fn rows_keep_their_order_across_batches() {
    let rt = DbRuntime::new().unwrap();
    let s = session(&rt).await;
    let events = collect(s.execute("rows 5000")).await;
    assert_eq!(rows_of(&events), (0..5000).collect::<Vec<_>>());
}

#[tokio::test]
async fn slow_rows_are_not_held_back_waiting_for_a_full_batch() {
    let rt = DbRuntime::new().unwrap();
    let s = session(&rt).await;
    let mut handle = s.execute("trickle 3 300");
    let started = Instant::now();
    loop {
        match handle.next().await.unwrap() {
            QueryEvent::Rows(rows) => {
                assert_eq!(rows.len(), 1);
                assert!(
                    started.elapsed() < Duration::from_millis(700),
                    "first row took {:?}",
                    started.elapsed()
                );
                break;
            }
            QueryEvent::Finished(r) => panic!("finished before any rows: {r:?}"),
            _ => {}
        }
    }
}

#[tokio::test]
async fn connect_times_out() {
    let rt = DbRuntime::new().unwrap();
    let cfg = ConnectConfig {
        connect_timeout: Some(Duration::from_millis(100)),
        ..config("slow")
    };
    let err = rt
        .connect(
            Arc::new(FakeDriver::default()),
            cfg,
            SessionOptions::default(),
        )
        .await
        .err()
        .unwrap();
    assert!(matches!(err, DbError::Connect(m) if m.contains("timed out")));
}

#[tokio::test]
async fn cancelling_a_queued_query_skips_it() {
    let rt = DbRuntime::new().unwrap();
    let s = session(&rt).await;
    let running = s.execute("sleep 200");
    let queued = s.execute("update 1");
    queued.cancel();
    let (_, events) = tokio::join!(collect(running), collect(queued));
    assert_eq!(events, vec![QueryEvent::Finished(Err(DbError::Cancelled))]);
}

#[tokio::test]
async fn dropping_the_handle_mid_stream_leaves_the_session_usable() {
    let rt = DbRuntime::new().unwrap();
    let s = session(&rt).await;
    let mut handle = s.execute("rows 1000000");
    // read a little, then walk away
    while !matches!(handle.next().await, Some(QueryEvent::Rows(_))) {}
    drop(handle);
    let events = collect(s.execute("update 7")).await;
    assert!(matches!(
        events[0],
        QueryEvent::Done {
            rows_affected: Some(7)
        }
    ));
}

#[tokio::test]
async fn closing_reaches_the_driver() {
    let rt = DbRuntime::new().unwrap();
    let driver = Arc::new(FakeDriver::default());
    let s = rt
        .connect(driver.clone(), config("dev"), SessionOptions::default())
        .await
        .unwrap();
    s.close().await;
    assert_eq!(driver.closed_count(), 1);

    // dropping every clone closes too
    let s = rt
        .connect(driver.clone(), config("dev"), SessionOptions::default())
        .await
        .unwrap();
    let clone = s.clone();
    drop(s);
    drop(clone);
    for _ in 0..50 {
        if driver.closed_count() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(driver.closed_count(), 2);
}

#[test]
fn debug_output_hides_the_password() {
    let cfg = ConnectConfig {
        password: Some("hunter2".into()),
        ..config("dev")
    };
    let shown = format!("{cfg:?}");
    assert!(!shown.contains("hunter2"));
    assert!(shown.contains("***"));
}

#[test]
fn only_connection_loss_is_fatal() {
    assert!(DbError::ConnectionLost("x".into()).is_fatal());
    assert!(!DbError::query("x").is_fatal());
    assert!(!DbError::Cancelled.is_fatal());
}

#[test]
fn usable_from_a_non_tokio_executor() {
    // gpui drives these futures on its own executor, not tokio
    let rt = DbRuntime::new().unwrap();
    let events = std::thread::spawn(move || {
        pollster_block_on(async {
            let s = session(&rt).await;
            collect(s.execute("rows 3")).await
        })
    })
    .join()
    .unwrap();
    assert!(finished(&events).is_ok());
}

/// Minimal block_on with no runtime, standing in for a foreign executor.
fn pollster_block_on<F: Future>(fut: F) -> F::Output {
    use std::sync::{Condvar, Mutex};
    use std::task::{Context, Poll, Wake, Waker};

    struct Signal(Mutex<bool>, Condvar);
    impl Wake for Signal {
        fn wake(self: Arc<Self>) {
            *self.0.lock().unwrap() = true;
            self.1.notify_one();
        }
    }

    let signal = Arc::new(Signal(Mutex::new(false), Condvar::new()));
    let waker = Waker::from(signal.clone());
    let mut cx = Context::from_waker(&waker);
    let mut fut = std::pin::pin!(fut);
    loop {
        if let Poll::Ready(out) = fut.as_mut().poll(&mut cx) {
            return out;
        }
        let mut woken = signal.0.lock().unwrap();
        while !*woken {
            woken = signal.1.wait(woken).unwrap();
        }
        *woken = false;
    }
}
