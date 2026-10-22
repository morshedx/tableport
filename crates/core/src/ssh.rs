use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh::client;
use russh::keys::{self, HashAlg, PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use crate::DatabaseError;

/// An SSH server to tunnel the database connection through.
#[derive(Debug, Clone, PartialEq, Eq, Hash, uniffi::Record)]
pub struct SshSettings {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: Option<String>,
    /// An OpenSSH private key file; "~" means the home folder.
    pub private_key_path: Option<String>,
    pub key_passphrase: Option<String>,
    /// A host key fingerprint the user accepted, like "SHA256:…". Used when
    /// the host is not in ~/.ssh/known_hosts.
    pub trusted_fingerprint: Option<String>,
}

/// A local port that forwards every connection through SSH to the database
/// server. Dropping it closes the listener and the SSH session.
pub(crate) struct Tunnel {
    pub(crate) local_port: u16,
    listener_task: JoinHandle<()>,
    _session: Arc<client::Handle<HostKeyCheck>>,
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.listener_task.abort();
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Verdict {
    Trusted,
    Unknown,
    Changed,
}

/// Accepts the server key only if ~/.ssh/known_hosts has it or the user
/// trusted its fingerprint; remembers what it saw for the error message.
struct HostKeyCheck {
    host: String,
    port: u16,
    trusted_fingerprint: Option<String>,
    seen: Arc<Mutex<Option<(String, Verdict)>>>,
}

impl client::Handler for HostKeyCheck {
    type Error = russh::Error;

    async fn check_server_key(&mut self, key: &PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        // Host certificates aren't supported yet; they count as unknown keys.
        let PublicKeyOrCertificate::PublicKey { key, .. } = key else {
            self.record(String::from("(certificate)"), Verdict::Unknown);
            return Ok(false);
        };
        let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();
        let verdict = match keys::check_known_hosts(&self.host, self.port, key) {
            Ok(true) => Verdict::Trusted,
            Err(keys::Error::KeyChanged { .. }) => Verdict::Changed,
            _ if self.trusted_fingerprint.as_deref() == Some(fingerprint.as_str()) => Verdict::Trusted,
            _ => Verdict::Unknown,
        };
        self.record(fingerprint, verdict);
        Ok(verdict == Verdict::Trusted)
    }
}

impl HostKeyCheck {
    fn record(&self, fingerprint: String, verdict: Verdict) {
        if let Ok(mut seen) = self.seen.lock() {
            *seen = Some((fingerprint, verdict));
        }
    }
}

/// Connects to the SSH server, signs in, and starts forwarding a local port
/// to `target_host:target_port` as the SSH server sees it.
pub(crate) async fn open_tunnel(ssh: &SshSettings, target_host: &str, target_port: u16) -> Result<Tunnel, DatabaseError> {
    let endpoint = format!("{}:{}", ssh.host, ssh.port);
    let seen = Arc::new(Mutex::new(None));
    let check = HostKeyCheck {
        host: ssh.host.clone(),
        port: ssh.port,
        trusted_fingerprint: ssh.trusted_fingerprint.clone(),
        seen: seen.clone(),
    };
    let config = Arc::new(client::Config {
        keepalive_interval: Some(Duration::from_secs(30)),
        ..Default::default()
    });

    let connected = tokio::time::timeout(
        Duration::from_secs(10),
        client::connect(config, (ssh.host.as_str(), ssh.port), check),
    )
    .await
    .map_err(|_| DatabaseError::new(format!("Could not reach SSH server {endpoint}: no answer within the time limit.")))?;
    let mut session = match connected {
        Ok(session) => session,
        Err(error) => {
            let seen = seen.lock().ok().and_then(|seen| seen.clone());
            return Err(match seen {
                Some((fingerprint, Verdict::Unknown)) => {
                    DatabaseError::UnknownHostKey { host: endpoint, fingerprint }
                }
                Some((fingerprint, Verdict::Changed)) => DatabaseError::new(format!(
                    "The SSH host key of {endpoint} has changed ({fingerprint}). This can mean someone is \
                     intercepting the connection. Check the server, then update ~/.ssh/known_hosts."
                )),
                _ => DatabaseError::new(format!("Could not reach SSH server {endpoint}: {}.", reason(&error))),
            });
        }
    };

    let signed_in = if let Some(path) = ssh.private_key_path.as_deref().filter(|path| !path.is_empty()) {
        let key = keys::load_secret_key(expand_home(path), ssh.key_passphrase.as_deref()).map_err(|error| {
            DatabaseError::new(format!("Could not read the SSH key {path}: {error}."))
        })?;
        let hash = session.best_supported_rsa_hash().await.ok().flatten().flatten();
        session
            .authenticate_publickey(&ssh.user, PrivateKeyWithHashAlg::new(Arc::new(key), hash))
            .await
    } else if let Some(password) = &ssh.password {
        session.authenticate_password(&ssh.user, password).await
    } else {
        return Err(DatabaseError::new("SSH needs a password or a key file."));
    }
    .map_err(|error| DatabaseError::new(format!("SSH sign-in to {endpoint} failed: {}.", reason(&error))))?;
    if !signed_in.success() {
        return Err(DatabaseError::new(format!("SSH server {endpoint} did not accept user “{}” with this {}.",
            ssh.user, if ssh.private_key_path.as_deref().is_some_and(|path| !path.is_empty()) { "key" } else { "password" })));
    }

    let listener = TcpListener::bind((IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
        .await
        .map_err(|error| DatabaseError::new(format!("Could not open a local port for the SSH tunnel: {error}.")))?;
    let local_port = listener
        .local_addr()
        .map_err(|error| DatabaseError::new(format!("Could not open a local port for the SSH tunnel: {error}.")))?
        .port();
    let session = Arc::new(session);
    let listener_task = tokio::spawn({
        let session = session.clone();
        let target_host = target_host.to_string();
        async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let session = session.clone();
                let target_host = target_host.clone();
                tokio::spawn(async move {
                    let Ok(channel) = session
                        .channel_open_direct_tcpip(target_host, u32::from(target_port), "127.0.0.1", u32::from(local_port))
                        .await
                    else {
                        // Dropping the socket closes it; tokio-postgres then
                        // reports the connection as closed.
                        return;
                    };
                    let mut stream = channel.into_stream();
                    // Ends when either side closes; nothing to report.
                    drop(tokio::io::copy_bidirectional(&mut socket, &mut stream).await);
                });
            }
        }
    });
    Ok(Tunnel { local_port, listener_task, _session: session })
}

fn expand_home(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(path),
    }
}

fn reason(error: &russh::Error) -> String {
    match error {
        russh::Error::IO(io) if io.kind() == std::io::ErrorKind::ConnectionRefused => "Connection refused".into(),
        russh::Error::IO(io) => io.to_string(),
        other => other.to_string(),
    }
}
