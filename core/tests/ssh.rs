//! SSH tunnel tests need a throwaway sshd on 127.0.0.1:2222 that accepts the
//! current user with the keys in $TABLEPORT_TEST_SSH (client_key without a
//! passphrase, client_key_pw with "secret-pass"), and the local PostgreSQL.
//! The sshd config needs `PerSourcePenalties no`: these tests fail logins on
//! purpose, and OpenSSH 9.8+ would then refuse 127.0.0.1 for a while.

mod common;

use tableport_core::{test_connection, DatabaseError, DatabaseSession, SshSettings, TlsMode};

fn key_dir() -> Option<String> {
    std::env::var("TABLEPORT_TEST_SSH").ok().filter(|_| common::enabled())
}

/// One SSH test at a time: sshd's MaxStartups resets connections when many
/// arrive unauthenticated at once, which parallel tests would cause.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn ssh(dir: &str, key: &str, passphrase: Option<&str>, trusted: Option<String>) -> SshSettings {
    SshSettings {
        host: "127.0.0.1".into(),
        port: 2222,
        user: std::env::var("USER").unwrap_or_default(),
        password: None,
        private_key_path: Some(format!("{dir}/{key}")),
        key_passphrase: passphrase.map(str::to_string),
        trusted_fingerprint: trusted,
    }
}

fn through(ssh: SshSettings) -> tableport_core::ConnectionSettings {
    let mut settings = common::local("postgres", 5432, TlsMode::Disable);
    settings.ssh = Some(ssh);
    settings
}

/// The first attempt reports the unknown key; this returns its fingerprint.
async fn fingerprint(dir: &str) -> String {
    match test_connection(through(ssh(dir, "client_key", None, None))).await {
        Err(DatabaseError::UnknownHostKey { fingerprint, host }) => {
            assert_eq!(host, "127.0.0.1:2222");
            fingerprint
        }
        other => panic!("expected an unknown host key, got {other:?}"),
    }
}

#[tokio::test]
async fn unknown_host_key_is_refused_until_trusted() {
    let Some(dir) = key_dir() else { return };
    let _serial = SERIAL.lock().await;
    let fingerprint = fingerprint(&dir).await;
    assert!(fingerprint.starts_with("SHA256:"), "{fingerprint}");

    let session = DatabaseSession::connect(through(ssh(&dir, "client_key", None, Some(fingerprint)))).await.unwrap();
    let result = session.run_query("SELECT current_user = session_user".into(), 10).await.unwrap();
    assert_eq!(result.result_sets[0].rows, vec![vec![Some("t".to_string())]]);
}

#[tokio::test]
async fn wrong_fingerprint_is_refused() {
    let Some(dir) = key_dir() else { return };
    let _serial = SERIAL.lock().await;
    let wrong = "SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".to_string();
    let error = test_connection(through(ssh(&dir, "client_key", None, Some(wrong)))).await.unwrap_err();
    assert!(matches!(error, DatabaseError::UnknownHostKey { .. }), "{error:?}");
}

#[tokio::test]
async fn key_with_passphrase() {
    let Some(dir) = key_dir() else { return };
    let _serial = SERIAL.lock().await;
    let trusted = Some(fingerprint(&dir).await);
    let error = test_connection(through(ssh(&dir, "client_key_pw", Some("wrong"), trusted.clone()))).await.unwrap_err();
    assert!(error.message().contains("Could not read the SSH key"), "{error}");
    let version = test_connection(through(ssh(&dir, "client_key_pw", Some("secret-pass"), trusted))).await.unwrap();
    assert!(!version.is_empty());
}

#[tokio::test]
async fn wrong_user_is_refused() {
    let Some(dir) = key_dir() else { return };
    let _serial = SERIAL.lock().await;
    let mut settings = ssh(&dir, "client_key", None, Some(fingerprint(&dir).await));
    settings.user = "tableport-no-such-user".into();
    let error = test_connection(through(settings)).await.unwrap_err();
    assert!(error.message().contains("did not accept user"), "{error}");
}

#[tokio::test]
async fn cancel_works_through_the_tunnel() {
    let Some(dir) = key_dir() else { return };
    let _serial = SERIAL.lock().await;
    let trusted = Some(fingerprint(&dir).await);
    let session = DatabaseSession::connect(through(ssh(&dir, "client_key", None, trusted))).await.unwrap();
    let running = {
        let session = session.clone();
        tokio::spawn(async move { session.run_query("SELECT pg_sleep(10)".into(), 10).await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    session.cancel_query().await.unwrap();
    let error = running.await.unwrap().unwrap_err();
    assert!(error.message().contains("canceling statement"), "{error}");
}

#[tokio::test]
async fn closed_ssh_port_is_reported() {
    let Some(dir) = key_dir() else { return };
    let _serial = SERIAL.lock().await;
    let mut settings = ssh(&dir, "client_key", None, None);
    settings.port = 1;
    let error = test_connection(through(settings)).await.unwrap_err();
    assert_eq!(error.message(), "Could not reach SSH server 127.0.0.1:1: Connection refused.");
}
