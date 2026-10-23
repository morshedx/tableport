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

/// Which database engine a connection talks to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum Engine {
    Postgres,
    /// A database file on this Mac: `database` is its path; the server
    /// fields, TLS and SSH are not used.
    Sqlite,
    /// MySQL or MariaDB. `database` is optional: the sidebar shows every
    /// database of the server.
    Mysql,
    /// libSQL or Turso: SQLite on a server. `database` is its URL
    /// (libsql://…, https://…) and `password` its auth token.
    Libsql,
    /// Redis or Valkey. `user` is an ACL user (empty for the default), and
    /// `database` the logical database to start in ("0").
    Redis,
    /// MongoDB. `host` is a host name, or a whole connection string
    /// (mongodb://, mongodb+srv:// for Atlas) without its password.
    Mongo,
    /// SQL Server or Azure SQL. `database` is optional: the login's default
    /// database is used without it.
    Mssql,
    /// Apache Cassandra or ScyllaDB. `database` is an optional keyspace to
    /// start in: the sidebar shows every keyspace.
    Cassandra,
    /// Oracle Database. `database` is the service name; `host` may instead
    /// hold a whole connect descriptor, (description=…).
    Oracle,
    /// Cloudflare D1. `user` is the account ID, `database` the database ID
    /// and `password` an API token with D1 rights.
    D1,
    /// Google Cloud Firestore. `user` is the project ID, `database` the
    /// database ID, `password` a service account's JSON key, and `host` the
    /// emulator's host:port when it is used.
    Firestore,
}

impl Engine {
    /// SQLite's SQL, catalog and screens: a file, a libSQL server or D1.
    pub(crate) fn is_sqlite(self) -> bool {
        matches!(self, Engine::Sqlite | Engine::Libsql | Engine::D1)
    }
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
    pub engine: Engine,
}

impl ConnectionSettings {
    /// Where the connection goes, for messages: "host:port", or the file name.
    pub(crate) fn endpoint(&self) -> String {
        match self.engine {
            Engine::Postgres | Engine::Mysql | Engine::Redis | Engine::Mssql | Engine::Cassandra => format!("{}:{}", self.host, self.port),
            // A connection string's hosts: mongodb+srv://user@cluster0.example.net/db → cluster0.example.net.
            Engine::Mongo if self.host.contains("://") => {
                let rest = self.host.split_once("://").map_or(self.host.as_str(), |(_, rest)| rest);
                let rest = rest.rsplit_once('@').map_or(rest, |(_, hosts)| hosts);
                rest.split(['/', '?']).next().unwrap_or(rest).to_string()
            }
            Engine::Mongo => format!("{}:{}", self.host, self.port),
            // A descriptor's host: (description=…(host=adb.oraclecloud.com)…) → adb.oraclecloud.com.
            Engine::Oracle if self.host.trim_start().starts_with('(') => {
                let lower = self.host.to_ascii_lowercase();
                lower
                    .find("host=")
                    .map(|start| self.host[start + 5..].split(')').next().unwrap_or_default().trim().to_string())
                    .unwrap_or_else(|| "Oracle".into())
            }
            Engine::Oracle => format!("{}:{}", self.host, self.port),
            // The database ID's first part names it well enough in messages.
            Engine::Firestore if !self.host.trim().is_empty() => format!("the Firestore emulator at {}", self.host.trim()),
            Engine::Firestore => format!("Firestore project {}", self.user.trim()),
            Engine::D1 => format!("D1 database {}", self.database.trim().split('-').next().unwrap_or_default()),
            Engine::Sqlite => std::path::Path::new(&self.database)
                .file_name()
                .map_or_else(|| self.database.clone(), |name| name.to_string_lossy().into_owned()),
            // The URL's host: libsql://name-org.turso.io → name-org.turso.io.
            Engine::Libsql => {
                let url = self.database.trim();
                let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
                rest.split('/').next().unwrap_or(rest).to_string()
            }
        }
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
enum Link {
    Postgres(PgLink),
    Sqlite(crate::sqlite::SqliteLink),
    Mysql(crate::mysql::MysqlLink),
    Libsql(crate::libsql::LibsqlLink),
    Redis(crate::redis::RedisLink),
    Mongo(crate::mongo::MongoLink),
    Mssql(crate::mssql::MssqlLink),
    Cassandra(crate::cassandra::CassandraLink),
    Oracle(crate::oracle::OracleLink),
    D1(crate::d1::D1Link),
    Firestore(crate::firestore::FirestoreLink),
}

struct PgLink {
    client: Arc<Client>,
    server_version: String,
    /// Drives the socket; aborting it closes the connection.
    connection_task: JoinHandle<()>,
    /// The TLS setup of the connection, for the separate cancel request.
    tls: Option<postgres_native_tls::MakeTlsConnector>,
    /// Kept open as long as the connection; cancel requests use it too.
    _tunnel: Option<Tunnel>,
}

impl Drop for PgLink {
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
        match &*self.read_link() {
            Link::Postgres(link) => link.server_version.clone(),
            Link::Sqlite(link) => link.version.clone(),
            Link::Mysql(link) => link.version.clone(),
            Link::Libsql(link) => link.version.clone(),
            Link::Redis(link) => link.version.clone(),
            Link::Mongo(link) => link.version.clone(),
            Link::Mssql(link) => link.version.clone(),
            Link::Cassandra(link) => link.version.clone(),
            Link::Oracle(link) => link.version.clone(),
            Link::D1(link) => link.version.clone(),
            Link::Firestore(link) => link.version.clone(),
        }
    }

    pub fn engine(&self) -> Engine {
        self.settings.engine
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
        if self.settings.engine.is_sqlite() {
            return self.execute("SELECT 1".into()).await;
        }
        if let Some(link) = self.mysql() {
            return on_runtime(async move { link.ping().await }).await;
        }
        if let Ok(link) = self.redis() {
            return on_runtime(async move { link.ping().await }).await;
        }
        if let Ok(link) = self.mongo() {
            return on_runtime(async move { link.ping().await }).await;
        }
        if let Some(link) = self.mssql() {
            return on_runtime(async move { link.ping().await }).await;
        }
        if let Some(link) = self.cassandra() {
            return on_runtime(async move { link.ping().await }).await;
        }
        if let Some(link) = self.oracle() {
            return on_runtime(async move { link.ping().await }).await;
        }
        if let Some(link) = self.firestore() {
            return on_runtime(async move { link.ping().await }).await;
        }
        let client = self.client()?;
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
        match &*self.read_link() {
            Link::Postgres(link) => link.client.is_closed(),
            Link::Sqlite(link) => link.is_closed(),
            Link::Mysql(link) => link.is_closed(),
            Link::Libsql(link) => link.is_closed(),
            Link::Redis(link) => link.is_closed(),
            Link::Mongo(link) => link.is_closed(),
            Link::Mssql(link) => link.is_closed(),
            Link::Cassandra(link) => link.is_closed(),
            Link::Oracle(link) => link.is_closed(),
            Link::D1(link) => link.is_closed(),
            Link::Firestore(link) => link.is_closed(),
        }
    }

    pub fn close(&self) {
        match &*self.read_link() {
            Link::Postgres(link) => link.connection_task.abort(),
            Link::Sqlite(link) => link.close(),
            Link::Mysql(link) => link.close(),
            Link::Libsql(link) => link.close(),
            Link::Redis(link) => link.close(),
            Link::Mongo(link) => link.close(),
            Link::Mssql(link) => link.close(),
            Link::Cassandra(link) => link.close(),
            Link::Oracle(link) => link.close(),
            Link::D1(link) => link.close(),
            Link::Firestore(link) => link.close(),
        }
    }

    /// Runs SQL and discards any rows. Several statements separated by `;`
    /// run as one implicit transaction.
    pub async fn execute(&self, sql: String) -> Result<(), DatabaseError> {
        if let Some(link) = self.sqlite() {
            return on_runtime(async move { link.execute(sql).await }).await;
        }
        if let Some(link) = self.mysql() {
            return on_runtime(async move { link.execute(sql).await }).await;
        }
        if settings_engine(self) == Engine::Mongo {
            return self.mongo_run(sql, 0).await.map(|_| ());
        }
        if settings_engine(self) == Engine::Firestore {
            return self.firestore_run(sql, 0, false).await.map(|_| ());
        }
        if let Some(link) = self.mssql() {
            return on_runtime(async move { link.execute(sql).await }).await;
        }
        if let Some(link) = self.cassandra() {
            return on_runtime(async move { link.execute(sql).await }).await;
        }
        if let Some(link) = self.oracle() {
            return on_runtime(async move { link.execute(sql).await }).await;
        }
        let client = self.client()?;
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

    /// The PostgreSQL client; an error on another engine, for features
    /// that only PostgreSQL has.
    pub(crate) fn client(&self) -> Result<Arc<Client>, DatabaseError> {
        match &*self.read_link() {
            Link::Postgres(link) => Ok(link.client.clone()),
            Link::Sqlite(_) | Link::Mysql(_) | Link::Libsql(_) | Link::Redis(_) | Link::Mongo(_) | Link::Mssql(_) | Link::Cassandra(_) | Link::Oracle(_) | Link::D1(_) | Link::Firestore(_) => {
                Err(DatabaseError::new("This needs a PostgreSQL connection."))
            }
        }
    }

    /// The SQLite link: a file, or a libSQL server.
    pub(crate) fn sqlite(&self) -> Option<crate::libsql::SqliteLike> {
        match &*self.read_link() {
            Link::Sqlite(link) => Some(crate::libsql::SqliteLike::File(link.clone())),
            Link::Libsql(link) => Some(crate::libsql::SqliteLike::Remote(link.clone())),
            Link::D1(link) => Some(crate::libsql::SqliteLike::D1(link.clone())),
            Link::Postgres(_) | Link::Mysql(_) | Link::Redis(_) | Link::Mongo(_) | Link::Mssql(_) | Link::Cassandra(_) | Link::Oracle(_) | Link::Firestore(_) => None,
        }
    }

    /// The MongoDB link; an error for the other engines.
    pub(crate) fn mongo(&self) -> Result<crate::mongo::MongoLink, DatabaseError> {
        match &*self.read_link() {
            Link::Mongo(link) => Ok(link.clone()),
            _ => Err(DatabaseError::new("This needs a MongoDB connection.")),
        }
    }

    /// The Redis link; an error for the SQL engines.
    pub(crate) fn redis(&self) -> Result<crate::redis::RedisLink, DatabaseError> {
        match &*self.read_link() {
            Link::Redis(link) => Ok(link.clone()),
            _ => Err(DatabaseError::new("This needs a Redis connection.")),
        }
    }

    /// The Firestore link, when the connection is to Firestore.
    pub(crate) fn firestore(&self) -> Option<crate::firestore::FirestoreLink> {
        match &*self.read_link() {
            Link::Firestore(link) => Some(link.clone()),
            _ => None,
        }
    }

    /// The Oracle link, when the connection is to Oracle Database.
    pub(crate) fn oracle(&self) -> Option<crate::oracle::OracleLink> {
        match &*self.read_link() {
            Link::Oracle(link) => Some(link.clone()),
            _ => None,
        }
    }

    /// The Cassandra link, when the connection is to Cassandra or ScyllaDB.
    pub(crate) fn cassandra(&self) -> Option<crate::cassandra::CassandraLink> {
        match &*self.read_link() {
            Link::Cassandra(link) => Some(link.clone()),
            _ => None,
        }
    }

    /// The SQL Server link, when the connection is to SQL Server or Azure SQL.
    pub(crate) fn mssql(&self) -> Option<crate::mssql::MssqlLink> {
        match &*self.read_link() {
            Link::Mssql(link) => Some(link.clone()),
            _ => None,
        }
    }

    /// The MySQL link, when the connection is to MySQL or MariaDB.
    pub(crate) fn mysql(&self) -> Option<crate::mysql::MysqlLink> {
        match &*self.read_link() {
            Link::Mysql(link) => Some(link.clone()),
            Link::Postgres(_) | Link::Sqlite(_) | Link::Libsql(_) | Link::Redis(_) | Link::Mongo(_) | Link::Mssql(_) | Link::Cassandra(_) | Link::Oracle(_) | Link::D1(_) | Link::Firestore(_) => None,
        }
    }

    pub(crate) fn dialect(&self) -> crate::rows::Dialect {
        match self.settings.engine {
            Engine::Postgres => crate::rows::Dialect::Postgres,
            Engine::Sqlite | Engine::Libsql | Engine::D1 => crate::rows::Dialect::Sqlite,
            Engine::Mysql => crate::rows::Dialect::Mysql,
            Engine::Mssql => crate::rows::Dialect::Mssql,
            Engine::Oracle => crate::rows::Dialect::Oracle,
            // Redis, MongoDB and Cassandra have their own paths; nothing asks for their dialect.
            Engine::Redis | Engine::Mongo | Engine::Cassandra | Engine::Firestore => crate::rows::Dialect::Postgres,
        }
    }

    pub(crate) fn tls(&self) -> Option<postgres_native_tls::MakeTlsConnector> {
        match &*self.read_link() {
            Link::Postgres(link) => link.tls.clone(),
            Link::Sqlite(_) | Link::Mysql(_) | Link::Libsql(_) | Link::Redis(_) | Link::Mongo(_) | Link::Mssql(_) | Link::Cassandra(_) | Link::Oracle(_) | Link::D1(_) | Link::Firestore(_) => None,
        }
    }

    fn read_link(&self) -> std::sync::RwLockReadGuard<'_, Link> {
        // Nothing panics while holding the lock; recover the guard anyway
        // rather than fail every later call.
        self.link.read().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn settings_engine(session: &DatabaseSession) -> Engine {
    session.settings.engine
}

fn poisoned() -> DatabaseError {
    DatabaseError::new("The connection state is unavailable after an earlier failure.")
}

async fn open_link(settings: &ConnectionSettings) -> Result<Link, DatabaseError> {
    match settings.engine {
        Engine::Postgres => open_pg_link(settings).await.map(Link::Postgres),
        Engine::Sqlite => crate::sqlite::SqliteLink::open(settings).await.map(Link::Sqlite),
        Engine::Mysql => crate::mysql::MysqlLink::open(settings).await.map(Link::Mysql),
        Engine::Libsql => crate::libsql::LibsqlLink::open(settings).await.map(Link::Libsql),
        Engine::Redis => crate::redis::RedisLink::open(settings).await.map(Link::Redis),
        Engine::Mongo => crate::mongo::MongoLink::open(settings).await.map(Link::Mongo),
        Engine::Mssql => crate::mssql::MssqlLink::open(settings).await.map(Link::Mssql),
        Engine::Cassandra => crate::cassandra::CassandraLink::open(settings).await.map(Link::Cassandra),
        Engine::Oracle => crate::oracle::OracleLink::open(settings).await.map(Link::Oracle),
        Engine::D1 => crate::d1::D1Link::open(settings).await.map(Link::D1),
        Engine::Firestore => crate::firestore::FirestoreLink::open(settings).await.map(Link::Firestore),
    }
}

async fn open_pg_link(settings: &ConnectionSettings) -> Result<PgLink, DatabaseError> {
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

    Ok(PgLink { client: Arc::new(client), server_version, connection_task, tls, _tunnel: tunnel })
}

impl DatabaseSession {

    /// Runs one statement through the simple query protocol, so every value
    /// arrives as text (None for NULL), whatever its type.
    pub(crate) async fn text_rows(&self, sql: String) -> Result<Vec<Vec<Option<String>>>, DatabaseError> {
        if let Some(link) = self.sqlite() {
            return on_runtime(async move { link.text_rows(sql).await }).await;
        }
        if let Some(link) = self.mysql() {
            return on_runtime(async move { link.text_rows(sql).await }).await;
        }
        if let Some(link) = self.mssql() {
            return on_runtime(async move { link.text_rows(sql).await }).await;
        }
        if let Some(link) = self.cassandra() {
            return on_runtime(async move { link.text_rows(sql).await }).await;
        }
        if let Some(link) = self.oracle() {
            return on_runtime(async move { link.text_rows(sql).await }).await;
        }
        let client = self.client()?;
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
