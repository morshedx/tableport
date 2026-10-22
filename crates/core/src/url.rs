use crate::{DatabaseError, TlsMode};

/// What a `postgres://` URL says. Missing parts stay None, so the form keeps
/// its own values for them.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ParsedUrl {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub database: Option<String>,
    pub user: Option<String>,
    pub password: Option<String>,
    pub tls: Option<TlsMode>,
}

/// Reads `postgres://user:password@host:port/database?sslmode=…`, also with
/// `postgresql://` and percent-encoded parts, as libpq does.
///
/// Read by hand rather than with tokio-postgres' parser, which refuses the
/// whole URL for any parameter it doesn't know. Real URLs carry many: Prisma
/// adds `schema=public`, poolers add `pgbouncer=true`, Heroku and Render use
/// `sslmode=no-verify`. Unknown parameters are skipped.
#[uniffi::export]
pub fn parse_connection_url(url: &str) -> Result<ParsedUrl, DatabaseError> {
    let url = url.trim();
    let rest = url
        .strip_prefix("postgresql://")
        .or_else(|| url.strip_prefix("postgres://"))
        .ok_or_else(|| DatabaseError::new("A connection URL starts with postgres:// or postgresql://."))?;
    let (rest, query) = rest.split_once('?').unwrap_or((rest, ""));
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));

    // The last @ ends the credentials, so an unencoded @ in a password works.
    let (credentials, hosts) = match authority.rsplit_once('@') {
        Some((credentials, hosts)) => (Some(credentials), hosts),
        None => (None, authority),
    };
    let mut parsed = ParsedUrl { host: None, port: None, database: None, user: None, password: None, tls: None };
    if let Some(credentials) = credentials {
        let (user, password) = match credentials.split_once(':') {
            Some((user, password)) => (user, Some(password)),
            None => (credentials, None),
        };
        parsed.user = non_empty(decode(user)?);
        parsed.password = password.map(decode).transpose()?;
    }

    // Several hosts ("a:5432,b:5433") are for failover; the form takes the first.
    let first = hosts.split(',').next().unwrap_or("");
    let (host, port) = split_host_port(first)?;
    parsed.host = non_empty(decode(host)?);
    parsed.port = port;
    parsed.database = non_empty(decode(path)?);

    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let value = decode(value)?;
        match key {
            "sslmode" => parsed.tls = Some(tls_mode(&value)?),
            "host" => parsed.host = non_empty(value),
            "port" => parsed.port = Some(parse_port(&value)?),
            "dbname" => parsed.database = non_empty(value),
            "user" => parsed.user = non_empty(value),
            "password" => parsed.password = Some(value),
            _ => {}
        }
    }
    // As libpq: no port means PostgreSQL's default.
    if parsed.host.is_some() && parsed.port.is_none() {
        parsed.port = Some(5432);
    }
    Ok(parsed)
}

/// `host`, `host:port`, `[::1]` or `[::1]:port`.
fn split_host_port(text: &str) -> Result<(&str, Option<u16>), DatabaseError> {
    if let Some(rest) = text.strip_prefix('[') {
        let (host, after) = rest
            .split_once(']')
            .ok_or_else(|| DatabaseError::new("This URL can't be read: an IPv6 host needs a closing ].".to_string()))?;
        let port = after.strip_prefix(':').map(parse_port).transpose()?;
        return Ok((host, port));
    }
    match text.rsplit_once(':') {
        Some((host, port)) => Ok((host, Some(parse_port(port)?))),
        None => Ok((text, None)),
    }
}

fn parse_port(text: &str) -> Result<u16, DatabaseError> {
    text.parse()
        .map_err(|_| DatabaseError::new(format!("This URL can't be read: “{text}” is not a port number.")))
}

/// libpq's sslmode values, and no-verify from Heroku and Render (TLS
/// without checking the certificate, which is what require does here).
fn tls_mode(value: &str) -> Result<TlsMode, DatabaseError> {
    Ok(match value {
        "disable" => TlsMode::Disable,
        "allow" | "prefer" => TlsMode::Prefer,
        "require" | "no-verify" => TlsMode::Require,
        "verify-ca" | "verify-full" => TlsMode::VerifyFull,
        other => return Err(DatabaseError::new(format!("This URL can't be read: “{other}” is not an sslmode."))),
    })
}

fn non_empty(text: String) -> Option<String> {
    (!text.is_empty()).then_some(text)
}

/// Percent-decoding, as in URLs; `+` stays a plus, as libpq reads it.
fn decode(text: &str) -> Result<String, DatabaseError> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = bytes
                .get(index + 1..index + 3)
                .and_then(|pair| std::str::from_utf8(pair).ok())
                .and_then(|pair| u8::from_str_radix(pair, 16).ok())
                .ok_or_else(|| DatabaseError::new("This URL can't be read: a % is not followed by two hex digits."))?;
            out.push(hex);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).map_err(|_| DatabaseError::new("This URL can't be read: it is not valid UTF-8 after decoding."))
}
