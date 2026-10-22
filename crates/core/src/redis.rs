//! Redis: keys and values, not tables. The app shows a key tree, a viewer
//! for each value type and a command console; this is everything behind
//! them. Keys are read with SCAN in pages, never KEYS, which blocks the
//! server while it walks every key.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use redis::aio::MultiplexedConnection;
use redis::{ConnectionAddr, ConnectionInfo, RedisConnectionInfo, Value};
use tokio::sync::Mutex;

use crate::session::{ConnectionSettings, TlsMode};
use crate::ssh::{open_tunnel, Tunnel};
use crate::{on_runtime, DatabaseError, DatabaseSession};

/// One logical database: its number and how many keys it holds.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct RedisDatabase {
    pub index: u32,
    pub keys: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum RedisKeyKind {
    String,
    Hash,
    List,
    Set,
    SortedSet,
    Stream,
    Json,
    /// A module's type, or one TablePort doesn't show.
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct RedisKey {
    pub name: String,
    pub kind: RedisKeyKind,
}

/// A page of a SCAN: `cursor` 0 means every key has been seen.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct RedisKeyPage {
    pub keys: Vec<RedisKey>,
    pub cursor: u64,
}

/// One element of a value: a hash field and its value, a list or set
/// member, a sorted set member and its score, or a stream entry.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct RedisItem {
    /// The hash field, the list index, or the stream entry ID.
    pub field: Option<String>,
    pub value: String,
    pub score: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct RedisValue {
    pub key: String,
    pub kind: RedisKeyKind,
    /// The type's name as Redis gives it, for kinds TablePort doesn't show.
    pub type_name: String,
    /// Seconds until the key expires; None when it never does.
    pub ttl_seconds: Option<i64>,
    /// Bytes of a string, elements of the others.
    pub length: u64,
    /// A string's or JSON document's text.
    pub text: Option<String>,
    /// The first elements of the others, up to the limit asked for.
    pub items: Vec<RedisItem>,
}

/// One change from the value viewer, reviewed as commands before it runs.
#[derive(Debug, Clone, PartialEq, uniffi::Enum)]
pub enum RedisChange {
    SetString { key: String, value: String },
    SetField { key: String, field: String, value: String },
    DeleteField { key: String, field: String },
    SetListItem { key: String, index: i64, value: String },
    PushListItem { key: String, value: String },
    /// Removes the first element equal to `value`.
    RemoveListItem { key: String, value: String },
    AddMember { key: String, member: String, score: Option<f64> },
    RemoveMember { key: String, member: String, sorted: bool },
    DeleteKey { key: String },
    RenameKey { key: String, new_name: String },
    /// None removes the expiry.
    SetTtl { key: String, seconds: Option<i64> },
}

#[derive(Clone)]
pub(crate) struct RedisLink {
    info: ConnectionInfo,
    /// One connection per logical database, opened on first use: SELECT on a
    /// shared connection would move every caller to another database.
    connections: Arc<Mutex<HashMap<u32, MultiplexedConnection>>>,
    /// "Redis 8.10.2".
    pub(crate) version: String,
    read_only: bool,
    endpoint: String,
    closed: Arc<AtomicBool>,
    _tunnel: Option<Arc<Tunnel>>,
}

/// Commands that only read, for read-only connections and AI access.
/// KEYS is left out: it blocks the server while it walks every key.
const READ_COMMANDS: &[&str] = &[
    "GET", "MGET", "STRLEN", "GETRANGE", "SUBSTR", "BITCOUNT", "GETBIT", "BITPOS", "HGET", "HMGET", "HGETALL", "HKEYS", "HVALS",
    "HLEN", "HEXISTS", "HSCAN", "HSTRLEN", "HRANDFIELD", "LRANGE", "LLEN", "LINDEX", "LPOS", "SMEMBERS", "SISMEMBER", "SMISMEMBER",
    "SCARD", "SSCAN", "SRANDMEMBER", "SINTER", "SUNION", "SDIFF", "SINTERCARD", "ZRANGE", "ZRANGEBYSCORE", "ZREVRANGE",
    "ZREVRANGEBYSCORE", "ZRANGEBYLEX", "ZREVRANGEBYLEX", "ZCARD", "ZSCORE", "ZMSCORE", "ZRANK", "ZREVRANK", "ZCOUNT", "ZLEXCOUNT",
    "ZSCAN", "ZRANDMEMBER", "ZINTER", "ZUNION", "ZDIFF", "ZINTERCARD", "XRANGE", "XREVRANGE", "XLEN", "XINFO", "TTL", "PTTL",
    "EXPIRETIME", "PEXPIRETIME", "TYPE", "EXISTS", "SCAN", "INFO", "DBSIZE", "PING", "ECHO", "TIME", "LASTSAVE", "MEMORY", "OBJECT",
    "GEOPOS", "GEODIST", "GEOHASH", "GEOSEARCH", "SORT_RO", "JSON.GET", "JSON.MGET", "JSON.TYPE", "JSON.STRLEN", "JSON.ARRLEN",
    "JSON.OBJKEYS", "JSON.OBJLEN", "LCS", "COMMAND",
];

/// Commands the console can't run on a shared connection, or that would
/// change which database or user the connection is.
const REFUSED_IN_CONSOLE: &[&str] = &[
    "SELECT", "MULTI", "EXEC", "DISCARD", "WATCH", "UNWATCH", "SUBSCRIBE", "PSUBSCRIBE", "SSUBSCRIBE", "UNSUBSCRIBE", "MONITOR",
    "SYNC", "PSYNC", "QUIT", "RESET", "HELLO", "AUTH", "CLIENT",
];

impl RedisLink {
    pub(crate) async fn open(settings: &ConnectionSettings) -> Result<Self, DatabaseError> {
        let endpoint = settings.endpoint();
        let tunnel = match &settings.ssh {
            Some(ssh) => Some(Arc::new(open_tunnel(ssh, &settings.host, settings.port).await?)),
            None => None,
        };
        let (host, port) = match &tunnel {
            Some(tunnel) => ("127.0.0.1".to_string(), tunnel.local_port),
            None => (settings.host.clone(), settings.port),
        };
        let mut redis_info = RedisConnectionInfo::default();
        if !settings.user.trim().is_empty() {
            redis_info = redis_info.set_username(settings.user.trim());
        }
        if let Some(password) = settings.password.as_deref().filter(|password| !password.is_empty()) {
            redis_info = redis_info.set_password(password);
        }
        let address = |tls: bool| -> ConnectionAddr {
            if tls {
                // "require" checks nothing; "verify-full" checks the certificate and name.
                ConnectionAddr::TcpTls { host: host.clone(), port, insecure: settings.tls != TlsMode::VerifyFull, tls_params: None }
            } else {
                ConnectionAddr::Tcp(host.clone(), port)
            }
        };
        let info = |tls: bool| -> Result<ConnectionInfo, DatabaseError> {
            use redis::IntoConnectionInfo;
            let info = address(tls).into_connection_info().map_err(|error| DatabaseError::new(error.to_string()))?;
            Ok(info.set_redis_settings(redis_info.clone()))
        };
        let mut link = Self {
            info: info(settings.tls != TlsMode::Disable)?,
            connections: Arc::new(Mutex::new(HashMap::new())),
            version: String::new(),
            read_only: settings.read_only,
            endpoint: endpoint.clone(),
            closed: Arc::new(AtomicBool::new(false)),
            _tunnel: tunnel,
        };
        let database: u32 = settings.database.trim().parse().unwrap_or(0);
        let info = match link.query(database, redis::cmd("INFO").arg("server").clone()).await {
            // TLS when the server has it, as libpq's "prefer".
            Err(_) if settings.tls == TlsMode::Prefer => {
                link.info = info(false)?;
                // The failed TLS attempt marked the link closed.
                link.closed.store(false, Ordering::Relaxed);
                link.connections.lock().await.clear();
                link.query(database, redis::cmd("INFO").arg("server").clone()).await?
            }
            other => other?,
        };
        let info = text(&info).unwrap_or_default();
        let field = |name: &str| {
            info.lines().find_map(|line| line.strip_prefix(&format!("{name}:")).map(|value| value.trim().to_string()))
        };
        // Valkey answers as Redis too, with server_name naming itself.
        let server = match field("server_name").as_deref() {
            Some("valkey") => "Valkey",
            _ => "Redis",
        };
        let number = field("valkey_version").or_else(|| field("redis_version")).unwrap_or_default();
        link.version = format!("{server} {number}");
        Ok(link)
    }

    async fn connection(&self, database: u32) -> Result<MultiplexedConnection, DatabaseError> {
        if self.closed.load(Ordering::Relaxed) {
            return Err(DatabaseError::new(format!("The connection to {} is closed.", self.endpoint)));
        }
        let mut connections = self.connections.lock().await;
        if let Some(connection) = connections.get(&database) {
            return Ok(connection.clone());
        }
        let info = self.info.clone();
        let redis = info.redis_settings().clone().set_db(i64::from(database));
        let client = redis::Client::open(info.set_redis_settings(redis)).map_err(|error| self.error(error))?;
        let connection = tokio::time::timeout(Duration::from_secs(10), client.get_multiplexed_async_connection())
            .await
            .map_err(|_| DatabaseError::new(format!("{} did not answer within 10 seconds.", self.endpoint)))?
            .map_err(|error| self.error(error))?;
        connections.insert(database, connection.clone());
        Ok(connection)
    }

    fn error(&self, error: redis::RedisError) -> DatabaseError {
        if error.is_io_error() || error.is_connection_dropped() || error.is_connection_refusal() {
            if error.is_connection_refusal() {
                return DatabaseError::new(format!("Could not reach {}: Connection refused.", self.endpoint));
            }
            self.closed.store(true, Ordering::Relaxed);
        }
        let detail = error.detail().map(str::to_string);
        let message = match (error.code(), detail) {
            (Some(code), Some(detail)) => format!("{code} {detail}"),
            _ => error.to_string(),
        };
        DatabaseError::new(message)
    }

    async fn query(&self, database: u32, command: redis::Cmd) -> Result<Value, DatabaseError> {
        let mut connection = self.connection(database).await?;
        command.query_async(&mut connection).await.map_err(|error| self.error(error))
    }

    async fn pipeline(&self, database: u32, pipeline: redis::Pipeline) -> Result<Vec<Value>, DatabaseError> {
        let mut connection = self.connection(database).await?;
        pipeline.query_async(&mut connection).await.map_err(|error| self.error(error))
    }

    pub(crate) async fn ping(&self) -> Result<(), DatabaseError> {
        match tokio::time::timeout(Duration::from_secs(5), self.query(0, redis::cmd("PING"))).await {
            Ok(result) => result.map(|_| ()),
            Err(_) => Err(DatabaseError::new(format!("{} did not answer within 5 seconds.", self.endpoint))),
        }
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
    }

    async fn databases(&self) -> Result<Vec<RedisDatabase>, DatabaseError> {
        // A server may refuse CONFIG (managed services do); 16 is Redis's default.
        let count = match self.query(0, redis::cmd("CONFIG").arg("GET").arg("databases").clone()).await {
            Ok(value) => config_number(&value).unwrap_or(16),
            Err(_) => 16,
        };
        let keyspace = self.query(0, redis::cmd("INFO").arg("keyspace").clone()).await?;
        let keyspace = text(&keyspace).unwrap_or_default();
        let keys: HashMap<u32, u64> = keyspace
            .lines()
            .filter_map(|line| {
                let (name, fields) = line.split_once(':')?;
                let index = name.strip_prefix("db")?.parse().ok()?;
                let keys = fields.split(',').find_map(|field| field.strip_prefix("keys="))?.parse().ok()?;
                Some((index, keys))
            })
            .collect();
        Ok((0..count).map(|index| RedisDatabase { index, keys: keys.get(&index).copied().unwrap_or(0) }).collect())
    }

    async fn scan(&self, database: u32, pattern: String, cursor: u64, count: u32) -> Result<RedisKeyPage, DatabaseError> {
        let pattern = if pattern.trim().is_empty() { "*".to_string() } else { pattern };
        let reply = self
            .query(database, redis::cmd("SCAN").arg(cursor).arg("MATCH").arg(&pattern).arg("COUNT").arg(count.max(10)).clone())
            .await?;
        let Value::Array(parts) = reply else { return Err(DatabaseError::new("SCAN answered with no cursor.")) };
        let next = parts.first().and_then(text).and_then(|cursor| cursor.parse().ok()).unwrap_or(0);
        let names: Vec<String> = match parts.get(1) {
            Some(Value::Array(names)) => names.iter().filter_map(text).collect(),
            _ => Vec::new(),
        };
        let mut types = redis::pipe();
        for name in &names {
            types.cmd("TYPE").arg(name);
        }
        let kinds = if names.is_empty() { Vec::new() } else { self.pipeline(database, types).await? };
        let keys = names
            .into_iter()
            .zip(kinds.iter().map(|kind| kind_of(&text(kind).unwrap_or_default())))
            .map(|(name, kind)| RedisKey { name, kind })
            .collect();
        Ok(RedisKeyPage { keys, cursor: next })
    }

    async fn value(&self, database: u32, key: String, limit: u32) -> Result<RedisValue, DatabaseError> {
        let mut header = redis::pipe();
        header.cmd("TYPE").arg(&key).cmd("PTTL").arg(&key);
        let header = self.pipeline(database, header).await?;
        let type_name = header.first().and_then(text).unwrap_or_default();
        if type_name == "none" {
            return Err(DatabaseError::new(format!("The key {key} no longer exists.")));
        }
        let ttl_seconds = match header.get(1) {
            Some(Value::Int(milliseconds)) if *milliseconds >= 0 => Some((milliseconds + 999) / 1000),
            _ => None,
        };
        let kind = kind_of(&type_name);
        let limit = i64::from(limit.max(1));
        let length_command = match kind {
            RedisKeyKind::String => Some("STRLEN"),
            RedisKeyKind::Hash => Some("HLEN"),
            RedisKeyKind::List => Some("LLEN"),
            RedisKeyKind::Set => Some("SCARD"),
            RedisKeyKind::SortedSet => Some("ZCARD"),
            RedisKeyKind::Stream => Some("XLEN"),
            RedisKeyKind::Json | RedisKeyKind::Other => None,
        };
        let length = match length_command {
            Some(command) => match self.query(database, redis::cmd(command).arg(&key).clone()).await? {
                Value::Int(length) => u64::try_from(length).unwrap_or(0),
                _ => 0,
            },
            None => 0,
        };
        let mut value = RedisValue { key: key.clone(), kind, type_name, ttl_seconds, length, text: None, items: Vec::new() };
        let item = |field: Option<String>, value: String, score: Option<f64>| RedisItem { field, value, score };
        match kind {
            RedisKeyKind::String => value.text = Some(display(&self.query(database, redis::cmd("GET").arg(&key).clone()).await?)),
            RedisKeyKind::Json => {
                value.text = Some(display(&self.query(database, redis::cmd("JSON.GET").arg(&key).clone()).await?));
            }
            RedisKeyKind::Hash => {
                value.items = scan_all(self, database, "HSCAN", &key, limit)
                    .await?
                    .chunks(2)
                    .map(|pair| item(pair.first().map(display), pair.get(1).map(display).unwrap_or_default(), None))
                    .collect();
            }
            RedisKeyKind::Set => {
                value.items = scan_all(self, database, "SSCAN", &key, limit).await?.iter().map(|member| item(None, display(member), None)).collect();
            }
            RedisKeyKind::List => {
                let values = self.query(database, redis::cmd("LRANGE").arg(&key).arg(0).arg(limit - 1).clone()).await?;
                value.items = array(values).iter().enumerate().map(|(index, member)| item(Some(index.to_string()), display(member), None)).collect();
            }
            RedisKeyKind::SortedSet => {
                let values = self.query(database, redis::cmd("ZRANGE").arg(&key).arg(0).arg(limit - 1).arg("WITHSCORES").clone()).await?;
                let values = array(values);
                value.items = if values.first().is_some_and(|first| matches!(first, Value::Array(_))) {
                    // RESP3 pairs each member with its score.
                    values
                        .iter()
                        .map(|pair| {
                            let pair = array(pair.clone());
                            item(None, pair.first().map(display).unwrap_or_default(), pair.get(1).and_then(number))
                        })
                        .collect()
                } else {
                    values.chunks(2).map(|pair| item(None, pair.first().map(display).unwrap_or_default(), pair.get(1).and_then(number))).collect()
                };
            }
            RedisKeyKind::Stream => {
                let values = self.query(database, redis::cmd("XRANGE").arg(&key).arg("-").arg("+").arg("COUNT").arg(limit).clone()).await?;
                value.items = array(values)
                    .into_iter()
                    .map(|entry| {
                        let entry = array(entry);
                        let id = entry.first().map(display);
                        let fields: serde_json::Map<String, serde_json::Value> = entry
                            .get(1)
                            .map(|fields| array(fields.clone()))
                            .unwrap_or_default()
                            .chunks(2)
                            .map(|pair| {
                                (pair.first().map(display).unwrap_or_default(), serde_json::Value::String(pair.get(1).map(display).unwrap_or_default()))
                            })
                            .collect();
                        item(id, serde_json::Value::Object(fields).to_string(), None)
                    })
                    .collect();
            }
            RedisKeyKind::Other => {}
        }
        Ok(value)
    }

    async fn apply(&self, database: u32, changes: Vec<RedisChange>) -> Result<(), DatabaseError> {
        if self.read_only {
            return Err(DatabaseError::new("This connection is read-only. Nothing was changed."));
        }
        let mut pipeline = redis::pipe();
        pipeline.atomic();
        for change in &changes {
            for command in commands(change)? {
                let mut cmd = redis::cmd(&command[0]);
                for argument in &command[1..] {
                    cmd.arg(argument);
                }
                pipeline.add_command(cmd);
            }
        }
        // One MULTI/EXEC: every command runs, or none if one is malformed.
        let results = self.pipeline(database, pipeline).await?;
        for result in &results {
            if let Value::ServerError(error) = result {
                return Err(DatabaseError::new(format!("{} {}", error.code(), error.details().unwrap_or_default())));
            }
        }
        Ok(())
    }

    /// The console: one command, its reply formatted as redis-cli shows it.
    async fn command(&self, database: u32, line: String, read_only: bool) -> Result<String, DatabaseError> {
        let arguments = split_command(&line)?;
        let Some(name) = arguments.first().map(|name| String::from_utf8_lossy(name).to_uppercase()) else {
            return Err(DatabaseError::new("Type a command."));
        };
        if REFUSED_IN_CONSOLE.contains(&name.as_str()) {
            let reason = if name == "SELECT" { "Pick the database in the sidebar." } else { "TablePort's console can't run it." };
            return Err(DatabaseError::new(format!("{name}: {reason}")));
        }
        if (read_only || self.read_only) && !is_read_command(&arguments) {
            return Err(DatabaseError::new(format!("{name} isn't allowed here: only commands that read are.")));
        }
        let mut command = redis::cmd(&name);
        for argument in &arguments[1..] {
            command.arg(argument.as_slice());
        }
        let reply = self.query(database, command).await?;
        if let Value::ServerError(error) = &reply {
            return Err(DatabaseError::new(format!("{} {}", error.code(), error.details().unwrap_or_default())));
        }
        Ok(format_reply(&reply, 0))
    }
}

/// Whether a command only reads. Some read commands write with an option:
/// SORT … STORE, GEOSEARCH … STORE(DIST), OBJECT/MEMORY subcommands are fine.
fn is_read_command(arguments: &[Vec<u8>]) -> bool {
    let Some(name) = arguments.first().map(|name| String::from_utf8_lossy(name).to_uppercase()) else { return false };
    if !READ_COMMANDS.contains(&name.as_str()) {
        return false;
    }
    !arguments.iter().skip(1).any(|argument| {
        let upper = String::from_utf8_lossy(argument).to_uppercase();
        upper == "STORE" || upper == "STOREDIST"
    })
}

/// SCAN-like commands (HSCAN, SSCAN) until `limit` elements.
async fn scan_all(link: &RedisLink, database: u32, command: &str, key: &str, limit: i64) -> Result<Vec<Value>, DatabaseError> {
    let per_element = if command == "HSCAN" { 2 } else { 1 };
    let mut cursor = "0".to_string();
    let mut out = Vec::new();
    loop {
        let reply = link.query(database, redis::cmd(command).arg(key).arg(&cursor).arg("COUNT").arg(200).clone()).await?;
        let parts = array(reply);
        cursor = parts.first().and_then(text).unwrap_or_else(|| "0".into());
        out.extend(parts.get(1).cloned().map(array).unwrap_or_default());
        if cursor == "0" || out.len() as i64 >= limit * per_element {
            break;
        }
    }
    out.truncate(usize::try_from(limit * per_element).unwrap_or(usize::MAX));
    Ok(out)
}

/// The commands a change runs, as arguments.
fn commands(change: &RedisChange) -> Result<Vec<Vec<String>>, DatabaseError> {
    let strings = |parts: &[&str]| parts.iter().map(|part| part.to_string()).collect::<Vec<_>>();
    Ok(match change {
        RedisChange::SetString { key, value } => {
            // KEEPTTL: changing the value keeps its expiry.
            vec![strings(&["SET", key, value, "KEEPTTL"])]
        }
        RedisChange::SetField { key, field, value } => vec![strings(&["HSET", key, field, value])],
        RedisChange::DeleteField { key, field } => vec![strings(&["HDEL", key, field])],
        RedisChange::SetListItem { key, index, value } => vec![strings(&["LSET", key, &index.to_string(), value])],
        RedisChange::PushListItem { key, value } => vec![strings(&["RPUSH", key, value])],
        RedisChange::RemoveListItem { key, value } => vec![strings(&["LREM", key, "1", value])],
        RedisChange::AddMember { key, member, score: Some(score) } => vec![strings(&["ZADD", key, &score.to_string(), member])],
        RedisChange::AddMember { key, member, score: None } => vec![strings(&["SADD", key, member])],
        RedisChange::RemoveMember { key, member, sorted } => vec![strings(&[if *sorted { "ZREM" } else { "SREM" }, key, member])],
        RedisChange::DeleteKey { key } => vec![strings(&["DEL", key])],
        RedisChange::RenameKey { key, new_name } => {
            if new_name.is_empty() {
                return Err(DatabaseError::new("The key needs a name."));
            }
            // RENAMENX: never overwrite another key.
            vec![strings(&["RENAMENX", key, new_name])]
        }
        RedisChange::SetTtl { key, seconds: Some(seconds) } => {
            if *seconds <= 0 {
                return Err(DatabaseError::new("The time to live must be at least one second."));
            }
            vec![strings(&["EXPIRE", key, &seconds.to_string()])]
        }
        RedisChange::SetTtl { key, seconds: None } => vec![strings(&["PERSIST", key])],
    })
}

/// The commands for review, as redis-cli lines.
#[uniffi::export]
pub fn redis_change_commands(changes: Vec<RedisChange>) -> Result<Vec<String>, DatabaseError> {
    let mut lines = Vec::new();
    for change in &changes {
        for command in commands(change)? {
            lines.push(command.iter().map(|argument| quote_argument(argument)).collect::<Vec<_>>().join(" "));
        }
    }
    Ok(lines)
}

/// An argument as redis-cli takes it: bare when it can be, else in quotes.
fn quote_argument(argument: &str) -> String {
    let plain = !argument.is_empty() && argument.chars().all(|c| c.is_ascii_graphic() && c != '"' && c != '\'' && c != '\\');
    if plain {
        return argument.to_string();
    }
    let mut quoted = String::from("\"");
    for character in argument.chars() {
        match character {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            other => quoted.push(other),
        }
    }
    quoted.push('"');
    quoted
}

/// A console line split into arguments, as redis-cli reads it: spaces
/// separate, "…" takes \" \\ \n \r \t \xHH escapes, '…' takes them literally.
/// Arguments are bytes: \xff is the byte 0xff, not the character ÿ.
pub(crate) fn split_command(line: &str) -> Result<Vec<Vec<u8>>, DatabaseError> {
    let mut arguments = Vec::new();
    let mut chars = line.trim().chars().peekable();
    while chars.peek().is_some() {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        let Some(&first) = chars.peek() else { break };
        let mut argument: Vec<u8> = Vec::new();
        if first == '"' || first == '\'' {
            chars.next();
            let mut closed = false;
            while let Some(character) = chars.next() {
                if character == first {
                    closed = true;
                    break;
                }
                if character == '\\' && first == '"' {
                    match chars.next() {
                        Some('n') => argument.push(b'\n'),
                        Some('r') => argument.push(b'\r'),
                        Some('t') => argument.push(b'\t'),
                        Some('x') => {
                            let hex: String = chars.by_ref().take(2).collect();
                            let byte = u8::from_str_radix(&hex, 16).map_err(|_| DatabaseError::new("\\x needs two hex digits."))?;
                            argument.push(byte);
                        }
                        Some(other) => push_char(&mut argument, other),
                        None => break,
                    }
                } else if character == '\\' && first == '\'' && chars.peek() == Some(&'\'') {
                    argument.push(b'\'');
                    chars.next();
                } else {
                    push_char(&mut argument, character);
                }
            }
            if !closed {
                return Err(DatabaseError::new("A quote is not closed."));
            }
        } else {
            while let Some(&character) = chars.peek() {
                if character.is_whitespace() {
                    break;
                }
                push_char(&mut argument, character);
                chars.next();
            }
        }
        arguments.push(argument);
    }
    Ok(arguments)
}

fn push_char(bytes: &mut Vec<u8>, character: char) {
    let mut buffer = [0; 4];
    bytes.extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
}

/// A reply as redis-cli shows it.
fn format_reply(value: &Value, indent: usize) -> String {
    match value {
        Value::Nil => "(nil)".into(),
        Value::Int(number) => format!("(integer) {number}"),
        Value::Double(number) => format!("(double) {number}"),
        Value::Boolean(flag) => format!("({flag})"),
        Value::Okay => "OK".into(),
        Value::SimpleString(text) => text.clone(),
        // redis-cli quotes every string reply, even a plain word.
        Value::BulkString(_) | Value::VerbatimString { .. } => {
            let quoted = quote_argument(&display(value));
            if quoted.starts_with('"') { quoted } else { format!("\"{quoted}\"") }
        }
        Value::Array(values) | Value::Set(values) => {
            if values.is_empty() {
                return "(empty array)".into();
            }
            let width = values.len().to_string().len();
            values
                .iter()
                .enumerate()
                .map(|(index, value)| {
                    let prefix = format!("{:>width$}) ", index + 1);
                    let body = format_reply(value, indent + prefix.len());
                    let line = format!("{prefix}{body}");
                    if index == 0 { line } else { format!("{}{line}", " ".repeat(indent)) }
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        Value::Map(pairs) => {
            if pairs.is_empty() {
                return "(empty hash)".into();
            }
            pairs
                .iter()
                .enumerate()
                .map(|(index, (key, value))| {
                    let line = format!("{}) {} => {}", index + 1, format_reply(key, indent), format_reply(value, indent + 4));
                    if index == 0 { line } else { format!("{}{line}", " ".repeat(indent)) }
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        Value::ServerError(error) => format!("(error) {} {}", error.code(), error.details().unwrap_or_default()),
        other => format!("{other:?}"),
    }
}

/// Text of a string-like reply.
fn text(value: &Value) -> Option<String> {
    match value {
        Value::BulkString(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
        Value::SimpleString(text) => Some(text.clone()),
        Value::VerbatimString { text, .. } => Some(text.clone()),
        Value::Int(number) => Some(number.to_string()),
        Value::Double(number) => Some(number.to_string()),
        Value::Okay => Some("OK".into()),
        _ => None,
    }
}

/// A value for the viewer: text when it is UTF-8, else redis-cli's \xHH
/// escapes for the bytes that aren't.
fn display(value: &Value) -> String {
    match value {
        Value::BulkString(bytes) => match std::str::from_utf8(bytes) {
            Ok(text) => text.to_string(),
            Err(_) => bytes
                .iter()
                .map(|byte| if byte.is_ascii_graphic() || *byte == b' ' { char::from(*byte).to_string() } else { format!("\\x{byte:02x}") })
                .collect(),
        },
        Value::Nil => String::new(),
        other => text(other).unwrap_or_else(|| format_reply(other, 0)),
    }
}

fn number(value: &Value) -> Option<f64> {
    match value {
        Value::Double(number) => Some(*number),
        other => text(other)?.parse().ok(),
    }
}

fn array(value: Value) -> Vec<Value> {
    match value {
        Value::Array(values) | Value::Set(values) => values,
        Value::Map(pairs) => pairs.into_iter().flat_map(|(key, value)| [key, value]).collect(),
        _ => Vec::new(),
    }
}

/// The number in CONFIG GET's answer: ["databases", "16"] or a map.
fn config_number(value: &Value) -> Option<u32> {
    array(value.clone()).get(1).and_then(text)?.parse().ok()
}

fn kind_of(type_name: &str) -> RedisKeyKind {
    match type_name {
        "string" => RedisKeyKind::String,
        "hash" => RedisKeyKind::Hash,
        "list" => RedisKeyKind::List,
        "set" => RedisKeyKind::Set,
        "zset" => RedisKeyKind::SortedSet,
        "stream" => RedisKeyKind::Stream,
        "ReJSON-RL" => RedisKeyKind::Json,
        _ => RedisKeyKind::Other,
    }
}

#[uniffi::export]
impl DatabaseSession {
    pub async fn redis_databases(&self) -> Result<Vec<RedisDatabase>, DatabaseError> {
        let link = self.redis()?;
        on_runtime(async move { link.databases().await }).await
    }

    /// One page of keys matching `pattern` (a glob, * for all); pass the
    /// returned cursor back for the next page, until it is 0.
    pub async fn redis_scan(&self, database: u32, pattern: String, cursor: u64, count: u32) -> Result<RedisKeyPage, DatabaseError> {
        let link = self.redis()?;
        on_runtime(async move { link.scan(database, pattern, cursor, count).await }).await
    }

    /// A key's type, expiry, length and first `limit` elements.
    pub async fn redis_value(&self, database: u32, key: String, limit: u32) -> Result<RedisValue, DatabaseError> {
        let link = self.redis()?;
        on_runtime(async move { link.value(database, key, limit).await }).await
    }

    /// Runs the changes in one MULTI/EXEC.
    pub async fn redis_apply(&self, database: u32, changes: Vec<RedisChange>) -> Result<(), DatabaseError> {
        let link = self.redis()?;
        on_runtime(async move { link.apply(database, changes).await }).await
    }

    /// The console: one command line, the reply as redis-cli shows it.
    pub async fn redis_command(&self, database: u32, line: String) -> Result<String, DatabaseError> {
        let link = self.redis()?;
        on_runtime(async move { link.command(database, line, false).await }).await
    }
}

impl DatabaseSession {
    /// For AI access: read commands only.
    pub(crate) async fn redis_read_command(&self, database: u32, line: String) -> Result<String, DatabaseError> {
        let link = self.redis()?;
        on_runtime(async move { link.command(database, line, true).await }).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_like_redis_cli() {
        let split = |line: &str| split_command(line).unwrap().into_iter().map(|argument| String::from_utf8(argument).unwrap()).collect::<Vec<_>>();
        assert_eq!(split("SET  key \"a b\\n\\x41\" 'it\\'s'"), ["SET", "key", "a b\nA", "it's"]);
        assert_eq!(split("  get k  "), ["get", "k"]);
        assert_eq!(split_command("SET b \"\\x00\\xff\"").unwrap()[2], [0x00, 0xff]);
        assert!(split_command("GET \"open").is_err());
        assert_eq!(quote_argument("plain"), "plain");
        assert_eq!(quote_argument("two words"), "\"two words\"");
        assert_eq!(quote_argument(""), "\"\"");
    }

    #[test]
    fn knows_which_commands_only_read() {
        let read = |line: &str| is_read_command(&split_command(line).unwrap());
        assert!(read("get a") && read("HGETALL h") && read("zrange z 0 -1"));
        assert!(!read("SET a b") && !read("DEL a") && !read("KEYS *") && !read("EVAL x 0") && !read("FLUSHALL"));
        assert!(!read("SORT_RO list STORE dest") && !read("GEOSEARCH g FROMLONLAT 0 0 BYRADIUS 1 km STORE x"));
    }

    #[test]
    fn formats_replies_as_redis_cli() {
        let reply = Value::Array(vec![Value::BulkString(b"a".to_vec()), Value::Array(vec![Value::Int(1), Value::Nil])]);
        assert_eq!(format_reply(&reply, 0), "1) \"a\"\n2) 1) (integer) 1\n   2) (nil)");
        assert_eq!(format_reply(&Value::Okay, 0), "OK");
        assert_eq!(format_reply(&Value::BulkString(b"x y".to_vec()), 0), "\"x y\"");
    }
}
