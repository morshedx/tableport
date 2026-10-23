use std::error::Error as _;
use std::io;

/// A failure with a message fit to show the user.
#[derive(Debug, Clone, PartialEq, thiserror::Error, uniffi::Error)]
pub enum DatabaseError {
    #[error("{message}")]
    Failed {
        message: String,
        detail: Option<String>,
        hint: Option<String>,
        /// 1-based character offset into the SQL the user ran, when the
        /// server reports one.
        position: Option<u32>,
    },
    /// The SSH server's key is neither in ~/.ssh/known_hosts nor trusted
    /// yet. The app asks the user to trust `fingerprint`, then connects with
    /// it as `trusted_fingerprint`.
    #[error("The SSH host key of {host} is not known yet ({fingerprint}).")]
    UnknownHostKey { host: String, fingerprint: String },
}

impl DatabaseError {
    pub fn new(message: impl Into<String>) -> Self {
        Self::Failed { message: message.into(), detail: None, hint: None, position: None }
    }

    pub fn message(&self) -> String {
        match self {
            Self::Failed { message, .. } => message.clone(),
            Self::UnknownHostKey { .. } => self.to_string(),
        }
    }

    /// The server's message when there is one; otherwise a short reason,
    /// prefixed with the endpoint for connection failures.
    pub(crate) fn from_postgres(error: tokio_postgres::Error, endpoint: &str) -> Self {
        if let Some(db) = error.as_db_error() {
            return Self::Failed {
                message: db.message().to_string(),
                detail: db.detail().map(str::to_string),
                hint: db.hint().map(str::to_string),
                position: match db.position() {
                    Some(tokio_postgres::error::ErrorPosition::Original(position)) => Some(*position),
                    _ => None,
                },
            };
        }
        // tokio-postgres keeps the reason in nested errors, for example
        // "error performing TLS handshake" ← "server does not support TLS".
        let mut text = error.to_string();
        let mut source = error.source();
        while let Some(cause) = source {
            text = format!("{text}: {cause}");
            source = cause.source();
        }
        if text.contains("does not support TLS") {
            return Self::Failed {
                message: format!("{endpoint} does not accept TLS connections."),
                detail: None,
                hint: Some("Set TLS to “disable” or “prefer”.".into()),
                position: None,
            };
        }
        if let Some(io) = error.source().and_then(|source| source.downcast_ref::<io::Error>()) {
            return Self::new(format!("Could not reach {endpoint}: {}.", io_reason(io)));
        }
        if text.contains("timeout") || text.contains("timed out") {
            return Self::new(format!("Could not reach {endpoint}: no answer within the time limit."));
        }
        if text.contains("password missing") {
            return Self::new("The server needs a password.");
        }
        if error.is_closed() {
            return Self::new("The server closed the connection.");
        }
        Self::new(text)
    }
}

impl DatabaseError {
    /// The server's message when there is one; otherwise a short reason,
    /// prefixed with the endpoint for connection failures.
    pub(crate) fn from_mysql(error: mysql_async::Error, endpoint: &str) -> Self {
        match error {
            mysql_async::Error::Server(server) => Self::new(server.message),
            mysql_async::Error::Io(mysql_async::IoError::Io(io)) => Self::new(format!("Could not reach {endpoint}: {}.", io_reason(&io))),
            mysql_async::Error::Io(mysql_async::IoError::Tls(tls)) => Self::new(format!("TLS with {endpoint} failed: {tls}")),
            mysql_async::Error::Driver(mysql_async::DriverError::NoClientSslFlagFromServer) => Self::Failed {
                message: format!("{endpoint} does not accept TLS connections."),
                detail: None,
                hint: Some("Set TLS to “disable” or “prefer”.".into()),
                position: None,
            },
            mysql_async::Error::Driver(mysql_async::DriverError::ConnectionClosed) => Self::new("The server closed the connection."),
            mysql_async::Error::Other(other) => Self::new(other.to_string()),
            other => Self::new(other.to_string()),
        }
    }
}

impl DatabaseError {
    /// The server's message when there is one; otherwise a short reason,
    /// prefixed with the endpoint for connection failures.
    pub(crate) fn from_mssql(error: tiberius::error::Error, endpoint: &str) -> Self {
        use tiberius::error::{Error, IoErrorKind};
        match error {
            Error::Server(server) => Self::new(server.message().to_string()),
            Error::Io { kind: IoErrorKind::ConnectionRefused, .. } => Self::new(format!("Could not reach {endpoint}: Connection refused.")),
            Error::Io { message, .. } => Self::new(format!("Could not reach {endpoint}: {message}.")),
            Error::Tls(message) if message.contains("certificate") || message.contains("verify") => Self::Failed {
                message: format!("TLS with {endpoint} failed: {message}"),
                detail: None,
                hint: Some("The server's certificate isn't trusted. Set TLS to “require” to skip the certificate check.".into()),
                position: None,
            },
            Error::Tls(message) => Self::new(format!("TLS with {endpoint} failed: {message}")),
            other => Self::new(other.to_string()),
        }
    }
}

impl DatabaseError {
    /// The driver's text, without the layers it wraps the server's message
    /// in: "Database returned an error: …, Error message: X" → "X".
    pub(crate) fn from_cassandra_text(text: &str, endpoint: &str) -> Self {
        if let Some((_, message)) = text.rsplit_once("Error message: ") {
            return Self::new(message.trim().to_string());
        }
        if text.contains("Connection refused") {
            return Self::new(format!("Could not reach {endpoint}: Connection refused."));
        }
        if text.contains("Authentication") || text.contains("authentication") {
            return Self::new(format!("{endpoint} refused the login: {text}"));
        }
        Self::new(text.to_string())
    }
}

fn io_reason(error: &io::Error) -> String {
    match error.kind() {
        io::ErrorKind::ConnectionRefused => "Connection refused".into(),
        io::ErrorKind::TimedOut => "no answer within the time limit".into(),
        _ => {
            let text = error.to_string();
            // getaddrinfo's wording differs by platform.
            if text.contains("lookup address") || text.contains("nodename nor servname") || text.contains("not known") {
                "the host name was not found".into()
            } else {
                text
            }
        }
    }
}
