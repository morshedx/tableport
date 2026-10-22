use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::task::JoinHandle;
use tokio_postgres::config::SslMode;
use tokio_postgres::{Client, NoTls, SimpleQueryMessage};

use crate::ssh::{open_tunnel, SshSettings, Tunnel};
use crate::{on_runtime, runtime, DatabaseError};

/// How the connection uses TLS, with libpq's `sslmode` meanings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum TlsMode {
    /// Plain TCP only.
    Disable,
    /// TLS when the server offers it, without certificate checks.
    Prefer,
    /// TLS always, without certificate checks.
    Require,
    /// TLS always, with the certificate chain and host name checked.
    VerifyFull,
}

/// Everything needed to open one connection.
#[derive(Debug, Clone, PartialEq, Eq, Hash, uniffi::Record)]
pub struct ConnectionSettings {
    pub host: String,
    pub port: u16,
    pub database: String,
    pub user: String,
    pub password: Option<String>,
    pub tls: TlsMode,
    /// When set, the connection goes through this SSH server, and `host` is
    /// the database server as the SSH server sees it.
    pub ssh: Option<SshSettings>,
    /// The server refuses every write in this session, including writes
    /// hidden in functions: it starts with default_transaction_read_only on.
    pub read_only: bool,
}

impl ConnectionSettings {
    pub(crate) fn endpoint(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

/// One open connection to a PostgreSQL server. The connection itself can be
/// replaced (Reconnect), so the app's tabs keep the same session object.
#[derive(uniffi::Object)]
pub struct DatabaseSession {
    settings: ConnectionSettings,
    link: RwLock<Link>,
}

/// One live connection: what Reconnect replaces.
struct Link {
    client: Arc<Client>,
    server_version: String,
    /// Drives the socket; aborting it closes the connection.
    connection_task: JoinHandle<()>,
    /// The TLS setup of the connection, for the separate cancel request.
    tls: Option<postgres_native_tls::MakeTlsConnector>,
    /// Kept open as long as the connection; cancel requests use it too.
    _tunnel: Option<Tunnel>,
}

impl Drop for Link {
    fn drop(&mut self) {
        self.connection_task.abort();
    }
}

#[uniffi::export]
impl DatabaseSession {
    /// Opens a connection and reads the server version.
    #[uniffi::constructor]
    pub async fn connect(settings: ConnectionSettings) -> Result<Arc<Self>, DatabaseError> {
        on_runtime(async move { Self::open(settings).await.map(Arc::new) }).await
    }

    /// For example "17.7 (Homebrew)".
    pub fn server_version(&self) -> String {
        self.read_link().server_version.clone()
    }

    /// Opens a new connection with the same settings (SSH tunnel included)
    /// and puts it in place of the old one. Running work on the old
    /// connection fails; later calls use the new one.
    pub async fn reconnect(&self) -> Result<(), DatabaseError> {
        let settings = self.settings.clone();
        let link = on_runtime(async move { open_link(&settings).await }).await?;
        let old = std::mem::replace(&mut *self.link.write().map_err(|_| poisoned())?, link);
        drop(old);
        Ok(())
    }

    /// A round trip to the server, failing after 5 seconds: tells whether
    /// the connection still works, for example after the Mac slept.
    pub async fn ping(&self) -> Result<(), DatabaseError> {
        let client = self.client();
        let endpoint = self.settings.endpoint();
        on_runtime(async move {
            match tokio::time::timeout(Duration::from_secs(5), client.simple_query("SELECT 1")).await {
                Ok(result) => result.map(|_| ()).map_err(|error| DatabaseError::from_postgres(error, &endpoint)),
                Err(_) => Err(DatabaseError::new(format!("{endpoint} did not answer within 5 seconds."))),
            }
        })
        .await
    }

    pub fn settings(&self) -> ConnectionSettings {
        self.settings.clone()
    }

    /// True once the connection has ended: the server closed it, or the
    /// network keepalives found it dead.
    pub fn is_closed(&self) -> bool {
        self.client().is_closed()
    }

    pub fn close(&self) {
        self.read_link().connection_task.abort();
    }

    /// Runs SQL and discards any rows. Several statements separated by `;`
    /// run as one implicit transaction.
    pub async fn execute(&self, sql: String) -> Result<(), DatabaseError> {
        let client = self.client();
        let endpoint = self.settings.endpoint();
        on_runtime(async move {
            client.batch_execute(&sql).await.map_err(|error| DatabaseError::from_postgres(error, &endpoint))
        })
        .await
    }
}

/// Connects, reads the server version, and disconnects.
#[uniffi::export]
pub async fn test_connection(settings: ConnectionSettings) -> Result<String, DatabaseError> {
    on_runtime(async move {
        let session = DatabaseSession::open(settings).await?;
        Ok(session.server_version())
    })
    .await
}

impl DatabaseSession {
    async fn open(settings: ConnectionSettings) -> Result<Self, DatabaseError> {
        let link = open_link(&settings).await?;
        Ok(Self { settings, link: RwLock::new(link) })
    }

    pub(crate) fn client(&self) -> Arc<Client> {
        self.read_link().client.clone()
    }

    pub(crate) fn tls(&self) -> Option<postgres_native_tls::MakeTlsConnector> {
        self.read_link().tls.clone()
    }

    fn read_link(&self) -> std::sync::RwLockReadGuard<'_, Link> {
        // Nothing panics while holding the lock; recover the guard anyway
        // rather than fail every later call.
        self.link.read().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn poisoned() -> DatabaseError {
    DatabaseError::new("The connection state is unavailable after an earlier failure.")
}

async fn open_link(settings: &ConnectionSettings) -> Result<Link, DatabaseError> {
    let endpoint = settings.endpoint();
    let mut config = tokio_postgres::Config::new();
    config
        .host(&settings.host)
        .user(&settings.user)
        .dbname(&settings.database)
        .application_name("tableport")
        .connect_timeout(Duration::from_secs(10))
        // Probes an idle connection, so one that died without a goodbye
        // (network change, sleep) is noticed in about a minute.
        .keepalives(true)
        .keepalives_idle(Duration::from_secs(30))
        .keepalives_interval(Duration::from_secs(10))
        .keepalives_retries(3)
        .ssl_mode(match settings.tls {
            TlsMode::Disable => SslMode::Disable,
            TlsMode::Prefer => SslMode::Prefer,
            TlsMode::Require | TlsMode::VerifyFull => SslMode::Require,
        });
    if let Some(password) = &settings.password {
        config.password(password);
    }
    if settings.read_only {
        config.options("-c default_transaction_read_only=on");
    }
    // Through SSH, connect to the tunnel's local port, but keep `host` for
    // TLS: verify-full still checks the real server name.
    let tunnel = match &settings.ssh {
        Some(ssh) => {
            let tunnel = open_tunnel(ssh, &settings.host, settings.port).await?;
            config.hostaddr(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
            Some(tunnel)
        }
        None => None,
    };
    // Set once: each call adds a port rather than replacing it.
    config.port(tunnel.as_ref().map_or(settings.port, |tunnel| tunnel.local_port));
    let map = |error| DatabaseError::from_postgres(error, &endpoint);
    let runtime = runtime()?;

    let (client, connection_task, tls) = if settings.tls == TlsMode::Disable {
        let (client, connection) = config.connect(NoTls).await.map_err(map)?;
        (client, runtime.spawn(async move { drop(connection.await) }), None)
    } else {
        let verify = settings.tls == TlsMode::VerifyFull;
        let connector = native_tls::TlsConnector::builder()
            .danger_accept_invalid_certs(!verify)
            .danger_accept_invalid_hostnames(!verify)
            .build()
            .map_err(|error| DatabaseError::new(format!("Could not set up TLS: {error}")))?;
        let tls = postgres_native_tls::MakeTlsConnector::new(connector);
        let (client, connection) = config.connect(tls.clone()).await.map_err(map)?;
        (client, runtime.spawn(async move { drop(connection.await) }), Some(tls))
    };

    let server_version = client
        .simple_query("SHOW server_version")
        .await
        .map_err(map)?
        .into_iter()
        .find_map(|message| match message {
            SimpleQueryMessage::Row(row) => row.get(0).map(str::to_string),
            _ => None,
        })
        .unwrap_or_default();

    Ok(Link { client: Arc::new(client), server_version, connection_task, tls, _tunnel: tunnel })
}

impl DatabaseSession {

    /// Runs one statement through the simple query protocol, so every value
    /// arrives as text (None for NULL), whatever its type.
    pub(crate) async fn text_rows(&self, sql: String) -> Result<Vec<Vec<Option<String>>>, DatabaseError> {
        let client = self.client();
        let endpoint = self.settings.endpoint();
        on_runtime(async move {
            let messages = client
                .simple_query(&sql)
                .await
                .map_err(|error| DatabaseError::from_postgres(error, &endpoint))?;
            Ok(messages
                .into_iter()
                .filter_map(|message| match message {
                    SimpleQueryMessage::Row(row) => {
                        Some((0..row.len()).map(|index| row.get(index).map(str::to_string)).collect())
                    }
                    _ => None,
                })
                .collect())
        })
        .await
    }
}
