mod common;

use std::time::{Duration, Instant};

use tableport_core::DatabaseError;
use common::{enabled, with_schema};

#[tokio::test]
async fn runs_several_statements() {
    if !enabled() { return; }
    with_schema(&["CREATE TABLE $S.t (id int, name text)"], |session, schema| async move {
        let result = session
            .run_query(
                format!(
                    "INSERT INTO {schema}.t VALUES (1, 'a'), (2, NULL);
                     SELECT id, name FROM {schema}.t ORDER BY id;
                     UPDATE {schema}.t SET name = 'z';
                     SELECT count(*) AS n FROM {schema}.t"
                ),
                100,
            )
            .await
            .unwrap();
        assert_eq!(result.statement_counts, [2, 2, 2, 1]);
        assert_eq!(result.statement_verbs, ["INSERT", "SELECT", "UPDATE", "SELECT"]);
        assert_eq!(result.result_sets.len(), 2);
        assert_eq!(result.result_sets[0].columns, ["id", "name"]);
        assert_eq!(result.result_sets[0].rows, vec![vec![Some("1".into()), Some("a".into())], vec![Some("2".into()), None]]);
        assert_eq!(result.result_sets[1].columns, ["n"]);
        assert_eq!(result.result_sets[1].rows, vec![vec![Some("2".into())]]);
    })
    .await;
}

#[tokio::test]
async fn keeps_only_max_rows_but_counts_all() {
    if !enabled() { return; }
    let session = common::connect().await;
    let result = session.run_query("SELECT generate_series(1, 2500) AS n".into(), 1000).await.unwrap();
    let set = &result.result_sets[0];
    assert_eq!(set.rows.len(), 1000);
    assert_eq!(set.total_rows, 2500);
}

#[tokio::test]
async fn error_reports_position() {
    if !enabled() { return; }
    let session = common::connect().await;
    let error = session.run_query("SELECT 1;\nSELECT nope FROM pg_class".into(), 10).await.unwrap_err();
    let DatabaseError::Failed { message, position, .. } = error else { panic!("unexpected {error:?}") };
    assert!(message.contains("column \"nope\" does not exist"), "{message}");
    // 1-based, counted from the start of the whole SQL: "nope" starts at character 18.
    assert_eq!(position, Some(18));
}

#[tokio::test]
async fn cancel_stops_a_running_query() {
    if !enabled() { return; }
    let session = common::connect().await;
    let start = Instant::now();
    let running = {
        let session = session.clone();
        tokio::spawn(async move { session.run_query("SELECT pg_sleep(10)".into(), 10).await })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    session.cancel_query().await.unwrap();
    let error = running.await.unwrap().unwrap_err();
    assert!(error.message().contains("canceling statement"), "{error}");
    assert!(start.elapsed() < Duration::from_secs(5), "took {:?}", start.elapsed());
    // The connection still works.
    let result = session.run_query("SELECT 1".into(), 10).await.unwrap();
    assert_eq!(result.result_sets[0].rows, vec![vec![Some("1".into())]]);
}

#[tokio::test]
async fn empty_sql_is_refused() {
    if !enabled() { return; }
    let session = common::connect().await;
    let error = session.run_query("  \n ".into(), 10).await.unwrap_err();
    assert_eq!(error.message(), "There is no SQL to run.");
}
