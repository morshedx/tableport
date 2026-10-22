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
