use crate::{DatabaseError, Engine, TlsMode};

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
    /// PostgreSQL for postgres://, MySQL for mysql:// and mariadb://, Redis
    /// for redis:// and rediss://, SQL Server for sqlserver:// and its
    /// connection strings.
    pub engine: Engine,
}

/// Reads `postgres://user:password@host:port/database?sslmode=…`, also with
/// `postgresql://` and percent-encoded parts, as libpq does; and
/// `mysql://` or `mariadb://` URLs with `ssl-mode`. SQL Server comes as
/// `sqlserver://`, a JDBC URL, or an ADO.NET connection string, as the Azure
/// portal shows it.
///
/// Read by hand rather than with tokio-postgres' parser, which refuses the
/// whole URL for any parameter it doesn't know. Real URLs carry many: Prisma
/// adds `schema=public`, poolers add `pgbouncer=true`, Heroku and Render use
/// `sslmode=no-verify`. Unknown parameters are skipped.
#[uniffi::export]
pub fn parse_connection_url(url: &str) -> Result<ParsedUrl, DatabaseError> {
    let url = url.trim();
    if let Some(rest) = url.strip_prefix("jdbc:sqlserver://") {
        return mssql_properties(&format!("server={}", rest.replacen(';', ";", 1)), true);
    }
    if !url.contains("://") && url.contains('=') {
        return mssql_properties(url, false);
    }
    let (engine, rest) = if let Some(rest) = url.strip_prefix("postgresql://").or_else(|| url.strip_prefix("postgres://")) {
        (Engine::Postgres, rest)
    } else if let Some(rest) = url.strip_prefix("mysql://").or_else(|| url.strip_prefix("mariadb://")) {
        (Engine::Mysql, rest)
    } else if let Some(rest) = url.strip_prefix("redis://") {
        (Engine::Redis, rest)
    } else if let Some(rest) = url.strip_prefix("rediss://") {
        (Engine::Redis, rest)
    } else if let Some(rest) = url.strip_prefix("sqlserver://").or_else(|| url.strip_prefix("mssql://")) {
        (Engine::Mssql, rest)
    } else if url.starts_with("mongodb://") || url.starts_with("mongodb+srv://") {
        return mongo_url(url);
    } else {
        return Err(DatabaseError::new(
            "A connection URL starts with postgres://, mysql://, mariadb://, sqlserver://, redis://, rediss:// or mongodb://.",
        ));
    };
    let (rest, query) = rest.split_once('?').unwrap_or((rest, ""));
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));

    // The last @ ends the credentials, so an unencoded @ in a password works.
    let (credentials, hosts) = match authority.rsplit_once('@') {
        Some((credentials, hosts)) => (Some(credentials), hosts),
        None => (None, authority),
    };
    let mut parsed = ParsedUrl { host: None, port: None, database: None, user: None, password: None, tls: None, engine };
    // rediss:// is Redis over TLS.
    if url.starts_with("rediss://") {
        parsed.tls = Some(TlsMode::Require);
    }
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

    let mut encrypt_flags = Vec::new();
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let value = decode(value)?;
        match key {
            "sslmode" => parsed.tls = Some(tls_mode(&value)?),
            // MySQL's client spells it ssl-mode, in capitals.
            "ssl-mode" | "ssl_mode" => parsed.tls = Some(tls_mode(&value.to_ascii_lowercase().replace('_', "-"))?),
            "host" => parsed.host = non_empty(value),
            "port" => parsed.port = Some(parse_port(&value)?),
            "dbname" => parsed.database = non_empty(value),
            "user" => parsed.user = non_empty(value),
            "password" => parsed.password = Some(value),
            "database" => parsed.database = non_empty(value),
            "encrypt" | "trustservercertificate" | "TrustServerCertificate" => {
                encrypt_flags.push((key.to_ascii_lowercase(), value.to_ascii_lowercase()));
            }
            _ => {}
        }
    }
    if !encrypt_flags.is_empty() {
        parsed.tls = Some(mssql_tls(&encrypt_flags));
    }
    // No port means the engine's default.
    if parsed.host.is_some() && parsed.port.is_none() {
        parsed.port = Some(match engine {
            Engine::Mysql => 3306,
            Engine::Redis => 6379,
            Engine::Mssql => 1433,
            _ => 5432,
        });
    }
    Ok(parsed)
}

/// A MongoDB connection string stays whole, as the host: its options and
/// several hosts mean something only to the driver. The password comes out,
/// for the Keychain; the default database is read from the path.
fn mongo_url(url: &str) -> Result<ParsedUrl, DatabaseError> {
    let (scheme, rest) = url.split_once("://").unwrap_or(("mongodb", url));
    let (authority, tail) = match rest.find(['/', '?']) {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, ""),
    };
    let (credentials, hosts) = match authority.rsplit_once('@') {
        Some((credentials, hosts)) => (Some(credentials), hosts),
        None => (None, authority),
    };
    let (user, password) = match credentials.map(|credentials| credentials.split_once(':').unwrap_or((credentials, ""))) {
        Some((user, password)) => (non_empty(decode(user)?), Some(decode(password)?).filter(|password| !password.is_empty())),
        None => (None, None),
    };
    let path = tail.strip_prefix('/').unwrap_or("");
    let database = non_empty(decode(path.split('?').next().unwrap_or(""))?);
    let without_password = match &user {
        Some(user) => format!("{scheme}://{}@{hosts}{tail}", user),
        None => format!("{scheme}://{hosts}{tail}"),
    };
    let tls = if tail.contains("tls=true") || tail.contains("ssl=true") || scheme == "mongodb+srv" { Some(TlsMode::VerifyFull) } else { None };
    Ok(ParsedUrl { host: Some(without_password), port: Some(0), database, user, password, tls, engine: Engine::Mongo })
}

/// An ADO.NET connection string (`Server=tcp:x.database.windows.net,1433;
/// Initial Catalog=db;User ID=…;Password=…;Encrypt=True;`), or a JDBC
/// URL's properties after its host (`jdbc` true).
fn mssql_properties(text: &str, jdbc: bool) -> Result<ParsedUrl, DatabaseError> {
    let mut parsed = ParsedUrl { host: None, port: None, database: None, user: None, password: None, tls: None, engine: Engine::Mssql };
    let mut encrypt_flags = Vec::new();
    for part in split_properties(text) {
        let Some((key, value)) = part.split_once('=') else { continue };
        let key = key.split_whitespace().collect::<Vec<_>>().join(" ").to_ascii_lowercase();
        let value = unquote(value.trim());
        match key.as_str() {
            "server" | "data source" | "address" | "addr" | "network address" => {
                let host = value.strip_prefix("tcp:").unwrap_or(&value);
                // ADO.NET puts the port after a comma; JDBC after a colon.
                let (host, port) = match host.rsplit_once(if jdbc { ':' } else { ',' }) {
                    Some((host, port)) => (host.trim(), Some(parse_port(port.trim())?)),
                    None => (host.trim(), None),
                };
                parsed.host = non_empty(host.to_string());
                parsed.port = port;
            }
            "port" | "portnumber" => parsed.port = Some(parse_port(&value)?),
            "initial catalog" | "database" | "databasename" => parsed.database = non_empty(value),
            "user id" | "uid" | "user" => parsed.user = non_empty(value),
            "password" | "pwd" => parsed.password = Some(value),
            "encrypt" | "trustservercertificate" | "trust server certificate" => {
                encrypt_flags.push((key.replace(' ', ""), value.to_ascii_lowercase()));
            }
            _ => {}
        }
    }
    if parsed.host.is_none() {
        return Err(DatabaseError::new("This connection string has no Server."));
    }
    if parsed.port.is_none() {
        parsed.port = Some(1433);
    }
    if !encrypt_flags.is_empty() {
        parsed.tls = Some(mssql_tls(&encrypt_flags));
    }
    Ok(parsed)
}

/// Encrypt and TrustServerCertificate as TLS modes: encrypted with a
/// trusted certificate is verify-full; Encrypt=False still encrypts when
/// the server asks for it, as SQL Server's drivers do.
fn mssql_tls(flags: &[(String, String)]) -> TlsMode {
    let value = |name: &str| flags.iter().rev().find(|(key, _)| key == name).map(|(_, value)| value.as_str());
    let yes = |text: Option<&str>| matches!(text, Some("true" | "yes" | "mandatory" | "strict"));
    match (yes(value("encrypt")), yes(value("trustservercertificate"))) {
        (false, _) => TlsMode::Prefer,
        (true, true) => TlsMode::Require,
        (true, false) => TlsMode::VerifyFull,
    }
}

/// `a=1;b={x;y};c="z"`, split at the semicolons outside braces and quotes.
fn split_properties(text: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut closing: Option<char> = None;
    for character in text.chars() {
        match (closing, character) {
            (None, ';') => parts.push(std::mem::take(&mut current)),
            (None, '{') => {
                closing = Some('}');
                current.push(character);
            }
            (None, '"' | '\'') => {
                closing = Some(character);
                current.push(character);
            }
            (Some(end), _) if character == end => {
                closing = None;
                current.push(character);
            }
            _ => current.push(character),
        }
    }
    parts.push(current);
    parts.into_iter().filter(|part| !part.trim().is_empty()).collect()
}

fn unquote(value: &str) -> String {
    for (open, close) in [('{', '}'), ('"', '"'), ('\'', '\'')] {
        if let Some(inner) = value.strip_prefix(open).and_then(|rest| rest.strip_suffix(close)) {
            return inner.to_string();
        }
    }
    value.to_string()
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
        "disable" | "disabled" => TlsMode::Disable,
        "allow" | "prefer" | "preferred" => TlsMode::Prefer,
        "require" | "required" | "no-verify" => TlsMode::Require,
        "verify-ca" | "verify-full" | "verify-identity" => TlsMode::VerifyFull,
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
