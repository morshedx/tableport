mod common;

use tableport_core::{test_connection, DatabaseSession, TlsMode};
use common::{enabled, local};

#[tokio::test]
async fn connect_reads_server_version() {
    if !enabled() { return; }
    let session = DatabaseSession::connect(local("postgres", 5432, TlsMode::Disable)).await.unwrap();
    assert!(session.server_version().starts_with(|c: char| c.is_ascii_digit()));
    session.close();
}

#[tokio::test]
async fn missing_database_shows_server_message() {
    if !enabled() { return; }
    let error = test_connection(local("tableport_no_such_database", 5432, TlsMode::Disable)).await.unwrap_err();
    assert!(error.message().contains("does not exist"), "{error}");
}

#[tokio::test]
async fn closed_port_says_refused_quickly() {
    if !enabled() { return; }
    let start = std::time::Instant::now();
    let error = test_connection(local("postgres", 1, TlsMode::Disable)).await.unwrap_err();
    assert_eq!(error.message(), "Could not reach localhost:1: Connection refused.");
    assert!(start.elapsed().as_secs() < 2, "took {:?}", start.elapsed());
}

#[tokio::test]
async fn unknown_host_says_not_found() {
    if !enabled() { return; }
    let mut settings = local("postgres", 5432, TlsMode::Disable);
    settings.host = "no-such-host.invalid".into();
    let error = test_connection(settings).await.unwrap_err();
    assert_eq!(error.message(), "Could not reach no-such-host.invalid:5432: the host name was not found.");
}

#[tokio::test]
async fn require_tls_fails_when_server_has_no_tls() {
    if !enabled() { return; }
    let error = test_connection(local("postgres", 5432, TlsMode::Require)).await.unwrap_err();
    assert!(error.message().contains("does not accept TLS"), "{error}");
}

#[tokio::test]
async fn prefer_tls_falls_back_to_plain() {
    if !enabled() { return; }
    let version = test_connection(local("postgres", 5432, TlsMode::Prefer)).await.unwrap();
    assert!(!version.is_empty());
}

#[tokio::test]
async fn bad_statement_shows_server_message() {
    if !enabled() { return; }
    let session = common::connect().await;
    let error = session.execute("SELEC 1".into()).await.unwrap_err();
    assert!(error.message().contains("syntax error"), "{error}");
}

#[tokio::test]
async fn read_only_session_refuses_writes() {
    if !enabled() { return; }
    let mut settings = local("postgres", 5432, TlsMode::Disable);
    settings.read_only = true;
    let session = DatabaseSession::connect(settings).await.unwrap();
    let error = session.execute("CREATE TEMP TABLE t (id int)".into()).await.unwrap_err();
    assert!(error.message().contains("read-only transaction"), "{error}");
    // Reads still work.
    let result = session.run_query("SELECT 1".into(), 10).await.unwrap();
    assert_eq!(result.result_sets[0].rows, vec![vec![Some("1".to_string())]]);
    // And the core refuses a commit before it reaches the server.
    let relation = tableport_core::Relation { schema: "public".into(), name: "x".into(), kind: tableport_core::RelationKind::Table };
    let error = session
        .commit(tableport_core::PendingChanges::default(), relation, vec![], vec![])
        .await
        .unwrap_err();
    assert!(error.message().contains("read-only"), "{error}");
}

#[tokio::test]
async fn notices_a_killed_connection_and_reconnects_in_place() {
    if !common::enabled() { return; }
    let session = common::connect().await;
    let pid = session.run_query("SELECT pg_backend_pid()::text".into(), 1).await.unwrap().result_sets[0].rows[0][0].clone().unwrap();
    // Another connection ends this one, as a server restart or an admin would.
    let other = common::connect().await;
    other.execute(format!("SELECT pg_terminate_backend({pid})")).await.unwrap();

    let mut closed = false;
    for _ in 0..50 {
        if session.ping().await.is_err() && session.is_closed() {
            closed = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(closed, "the dead connection is noticed");

    session.reconnect().await.unwrap();
    assert!(!session.is_closed());
    session.ping().await.unwrap();
    let new_pid = session.run_query("SELECT pg_backend_pid()::text".into(), 1).await.unwrap().result_sets[0].rows[0][0].clone().unwrap();
    assert_ne!(new_pid, pid, "a new server connection");
}
