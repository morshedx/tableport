//! Integration tests need a PostgreSQL server on localhost:5432 that trusts
//! the current macOS user (Homebrew's default). Run them with
//! `TABLEPORT_TEST_PG=1 cargo test`; without it they pass without running.

// Each test file uses a different part of this module.
#![allow(dead_code)]

use std::sync::Arc;

use tableport_core::{ConnectionSettings, DatabaseSession, TlsMode};

pub fn enabled() -> bool {
    std::env::var("TABLEPORT_TEST_PG").as_deref() == Ok("1")
}

pub fn local(database: &str, port: u16, tls: TlsMode) -> ConnectionSettings {
    ConnectionSettings {
        host: "localhost".into(),
        port,
        database: database.into(),
        user: std::env::var("USER").unwrap_or_else(|_| "postgres".into()),
        password: None,
        tls,
        ssh: None,
        read_only: false,
    }
}

pub async fn connect() -> Arc<DatabaseSession> {
    DatabaseSession::connect(local("postgres", 5432, TlsMode::Disable)).await.expect("connect to local PostgreSQL")
}

/// Runs `body` in a fresh schema, then drops it whether `body` passed or not.
pub async fn with_schema<F, Fut>(setup: &[&str], body: F)
where
    F: FnOnce(Arc<DatabaseSession>, String) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let session = connect().await;
    let schema = format!("tableport_test_{}_{}", std::process::id(), next_suffix());
    session.execute(format!("CREATE SCHEMA {schema}")).await.expect("create schema");
    for statement in setup {
        session.execute(statement.replace("$S", &schema)).await.expect("setup statement");
    }
    let result = futures_catch(body(session.clone(), schema.clone())).await;
    session.execute(format!("DROP SCHEMA {schema} CASCADE")).await.expect("drop schema");
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

/// Tests in one file run in parallel, so each needs its own schema name.
fn next_suffix() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

async fn futures_catch<Fut: std::future::Future<Output = ()>>(future: Fut) -> std::thread::Result<()> {
    use futures::FutureExt;
    std::panic::AssertUnwindSafe(future).catch_unwind().await
}
