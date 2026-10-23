//! Apache Cassandra (and ScyllaDB, which speaks the same CQL). Keyspaces
//! show as schemas, tables and materialized views as relations.
//!
//! CQL can't do much of what the grid asks of SQL: it has no OFFSET, sorts
//! only by clustering columns within one partition, and filters only some
//! columns, with ALLOW FILTERING. So the grid pages by reading on with the
//! driver's paging state, comparisons the server can do go to it, and the
//! rest of the filters run here, on the rows as they arrive. Values are
//! turned into text here too, the way cqlsh prints them.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use scylla::client::session::Session;
use scylla::client::session_builder::SessionBuilder;
use scylla::frame::response::result::{ColumnType, NativeType};
use scylla::response::{PagingState, PagingStateResponse};
use scylla::statement::Statement;
use scylla::value::{CqlValue, Row};
use tokio::sync::Notify;

use crate::changes::{CellDraft, PendingChanges, SqlFunction};
use crate::rows::{quote_identifier, quote_literal, Column, ColumnKind, FilterOperator, FilterTarget, RowFilter, RowPage, RowSort};
use crate::schema::{ForeignKey, Relation, RelationKind, SchemaContents};
use crate::session::{ConnectionSettings, TlsMode};
use crate::ssh::{open_tunnel, Tunnel};
use crate::structure::{non_empty, ColumnDetail, ConstraintDetail, ConstraintKind, IndexDetail, StructureChange, TableStructure};
use crate::{DatabaseError, DatabaseSession};

/// Keyspaces Cassandra keeps for itself; hidden from the sidebar.
const SYSTEM_KEYSPACES: &[&str] =
    &["system", "system_auth", "system_schema", "system_distributed", "system_traces", "system_views", "system_virtual_schema"];

/// Rows asked of the server at a time while the grid pages or filters.
const SCAN_PAGE: i32 = 1000;

#[derive(Clone)]
pub(crate) struct CassandraLink {
    session: Arc<Session>,
    /// Cancel: wakes the editor's running query, which is then dropped.
    stop: Arc<Notify>,
    /// "Cassandra 5.0.9".
    pub(crate) version: String,
    /// CQL has no read-only session; TablePort refuses writes itself.
    read_only: bool,
    closed: Arc<AtomicBool>,
    endpoint: String,
    /// Kept open as long as the session.
    _tunnel: Option<Arc<Tunnel>>,
}

/// One page of a statement's rows, with the column names and types.
struct Page {
    names: Vec<String>,
    types: Vec<ColumnType<'static>>,
    rows: Vec<Vec<Option<CqlValue>>>,
    next: Option<PagingState>,
}

impl CassandraLink {
    pub(crate) async fn open(settings: &ConnectionSettings) -> Result<Self, DatabaseError> {
        let endpoint = settings.endpoint();
        let tunnel = match &settings.ssh {
            Some(ssh) => Some(Arc::new(open_tunnel(ssh, &settings.host, settings.port).await?)),
            None => None,
        };
        let addresses = match tunnel {
            Some(_) => None,
            None => tokio::net::lookup_host((settings.host.as_str(), settings.port)).await.ok().map(|found| found.collect::<Vec<_>>()),
        };
        let connect = |use_tls: bool| {
            let mut builder = SessionBuilder::new()
                .connection_timeout(Duration::from_secs(10))
                // TablePort reads the schema tables itself, when it needs them.
                .fetch_schema_metadata(false)
                .tcp_keepalive_interval(Duration::from_secs(30));
            builder = match &tunnel {
                Some(tunnel) => {
                    let local = std::net::SocketAddr::from(([127, 0, 0, 1], tunnel.local_port));
                    // Every node the cluster names is reached through the tunnel's one node.
                    builder.known_node_addr(local).address_translator(Arc::new(ThroughTunnel(local)))
                }
                // Every address of the name: "localhost" is ::1 first, where Cassandra rarely listens.
                None => match &addresses {
                    Some(addresses) => builder.known_nodes_addr(addresses.iter().copied()),
                    None => builder.known_node(format!("{}:{}", settings.host, settings.port)),
                },
            };
            if !settings.user.is_empty() {
                builder = builder.user(settings.user.clone(), settings.password.clone().unwrap_or_default());
            }
            if let Some(keyspace) = Some(settings.database.trim()).filter(|name| !name.is_empty()) {
                builder = builder.use_keyspace(keyspace, false);
            }
            let tls = use_tls.then(|| tls_config(settings.tls == TlsMode::VerifyFull));
            async move {
                let builder = builder.tls_context(tls.transpose()?);
                match tokio::time::timeout(Duration::from_secs(15), builder.build()).await {
                    Ok(result) => result.map_err(|error| error.to_string()),
                    Err(_) => Err("no answer within 15 seconds".to_string()),
                }
            }
        };
        let session = match settings.tls {
            TlsMode::Disable => connect(false).await,
            TlsMode::Require | TlsMode::VerifyFull => connect(true).await,
            // CQL has no TLS negotiation: a port speaks TLS or it doesn't. Try TLS first.
            TlsMode::Prefer => match connect(true).await {
                Ok(session) => Ok(session),
                Err(_) => connect(false).await,
            },
        }
        .map_err(|error| DatabaseError::from_cassandra_text(&error, &endpoint))?;
        let link = Self {
            session: Arc::new(session),
            stop: Arc::new(Notify::new()),
            version: String::new(),
            read_only: settings.read_only,
            closed: Arc::new(AtomicBool::new(false)),
            endpoint,
            _tunnel: tunnel,
        };
        let rows = link.text_rows("SELECT release_version FROM system.local".into()).await?;
        let number = rows.into_iter().next().and_then(|row| row.into_iter().next().flatten()).unwrap_or_default();
        Ok(Self { version: format!("Cassandra {number}"), ..link })
    }

    fn fail(&self, error: impl std::fmt::Display) -> DatabaseError {
        let text = error.to_string();
        if text.contains("Broken pipe") || text.contains("Connection refused") || text.contains("connection broken") {
            self.closed.store(true, Ordering::Relaxed);
        }
        DatabaseError::from_cassandra_text(&text, &self.endpoint)
    }

    fn check_open(&self) -> Result<(), DatabaseError> {
        if self.is_closed() {
            return Err(DatabaseError::new(format!("The connection to {} is closed.", self.endpoint)));
        }
        Ok(())
    }

    fn refuse_writes(&self) -> Result<(), DatabaseError> {
        if self.read_only {
            return Err(DatabaseError::new("This connection is read-only. Nothing was changed."));
        }
        Ok(())
    }

    /// One page of `cql`, from `state`.
    async fn page(&self, cql: &str, size: i32, state: PagingState) -> Result<Page, DatabaseError> {
        self.check_open()?;
        let statement = Statement::new(cql).with_page_size(size);
        let (result, paging) = self.session.query_single_page(statement, &[], state).await.map_err(|error| self.fail(error))?;
        let next = match paging {
            PagingStateResponse::HasMorePages { state } => Some(state),
            PagingStateResponse::NoMorePages => None,
        };
        if !result.is_rows() {
            return Ok(Page { names: Vec::new(), types: Vec::new(), rows: Vec::new(), next });
        }
        let rows_result = result.into_rows_result().map_err(|error| self.fail(error))?;
        let specs = rows_result.column_specs();
        let names = specs.iter().map(|spec| spec.name().to_string()).collect();
        let types = specs.iter().map(|spec| spec.typ().clone().into_owned()).collect();
        let rows = rows_result
            .rows::<Row>()
            .map_err(|error| self.fail(error))?
            .map(|row| row.map(|row| row.columns))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| self.fail(error))?;
        Ok(Page { names, types, rows, next })
    }

    /// Reads `cql` to the end, passing each page to `each`, which returns
    /// false to stop early.
    async fn scan(&self, cql: &str, mut each: impl FnMut(&Page) -> bool) -> Result<(), DatabaseError> {
        let mut state = PagingState::start();
        loop {
            let page = self.page(cql, SCAN_PAGE, state).await?;
            if !each(&page) {
                return Ok(());
            }
            match page.next {
                Some(next) => state = next,
                None => return Ok(()),
            }
        }
    }

    /// Every row of `cql`, every value as text (None for null).
    pub(crate) async fn text_rows(&self, cql: String) -> Result<Vec<Vec<Option<String>>>, DatabaseError> {
        let mut rows = Vec::new();
        self.scan(&cql, |page| {
            rows.extend(page.rows.iter().map(|row| row.iter().map(|value| value.as_ref().map(text)).collect()));
            true
        })
        .await?;
        Ok(rows)
    }

    /// Runs one statement and discards any rows.
    pub(crate) async fn execute(&self, cql: String) -> Result<(), DatabaseError> {
        self.refuse_writes()?;
        self.check_open()?;
        for statement in crate::sql_text::statements(&cql).iter().filter(|statement| !statement.trim().is_empty()) {
            self.session.query_unpaged(statement.trim().to_string(), &[]).await.map_err(|error| self.fail(error))?;
        }
        Ok(())
    }

    pub(crate) async fn ping(&self) -> Result<(), DatabaseError> {
        match tokio::time::timeout(Duration::from_secs(5), self.page("SELECT key FROM system.local", 1, PagingState::start())).await {
            Ok(result) => result.map(|_| ()),
            Err(_) => Err(DatabaseError::new(format!("{} did not answer within 5 seconds.", self.endpoint))),
        }
    }

    pub(crate) fn cancel(&self) {
        self.stop.notify_waiters();
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
    }

    /// The query editor's Run: each statement in turn. CQL has no
    /// transactions, so a statement that fails leaves the ones before it done.
    /// Keeps the first `max_rows` rows of each result.
    pub(crate) async fn run_query(&self, cql: String, max_rows: u64) -> Result<crate::query::QueryResult, DatabaseError> {
        use crate::query::{cut, QueryResult, ResultSet};
        let statements: Vec<String> =
            crate::sql_text::statements(&cql).into_iter().map(|statement| statement.trim().to_string()).filter(|statement| !statement.is_empty()).collect();
        let verbs: Vec<String> = statements.iter().map(|statement| crate::sql_text::statement_verbs(statement).into_iter().next().unwrap_or_default()).collect();
        if self.read_only {
            if let Some(verb) = verbs.iter().find(|verb| !READ_VERBS.contains(&verb.as_str())) {
                return Err(DatabaseError::new(format!("This connection is read-only. {verb} was not run.")));
            }
        }
        let start = std::time::Instant::now();
        let stop = self.stop.notified();
        tokio::pin!(stop);
        stop.as_mut().enable();
        let work = async {
            let mut result_sets = Vec::new();
            let mut counts = Vec::new();
            for statement in &statements {
                let mut set: Option<ResultSet> = None;
                let mut state = PagingState::start();
                loop {
                    let page = self.page(statement, SCAN_PAGE, state).await?;
                    if !page.names.is_empty() {
                        let set = set.get_or_insert_with(|| ResultSet {
                            columns: page.names.clone(),
                            rows: Vec::new(),
                            total_rows: 0,
                            numeric_columns: page.types.iter().map(is_numeric_column).collect(),
                        });
                        for row in &page.rows {
                            if set.total_rows < max_rows {
                                set.rows.push(row.iter().map(|value| value.as_ref().map(|value| cut(&text(value)))).collect());
                            }
                            set.total_rows += 1;
                        }
                    }
                    match page.next {
                        Some(next) => state = next,
                        None => break,
                    }
                }
                counts.push(set.as_ref().map_or(0, |set| set.total_rows));
                result_sets.extend(set);
            }
            Ok::<_, DatabaseError>((result_sets, counts))
        };
        let (result_sets, statement_counts) = tokio::select! {
            outcome = work => outcome?,
            _ = &mut stop => return Err(DatabaseError::new("The query was canceled.")),
        };
        Ok(QueryResult {
            result_sets,
            statement_counts,
            statement_verbs: verbs,
            elapsed_ms: start.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
        })
    }

    /// One SELECT for AI access: CQL's SELECT can't write, so one SELECT is
    /// all the check needs. Reading stops after `max_rows` rows, or `timeout`.
    pub(crate) async fn read_only_json(&self, cql: &str, max_rows: u64, timeout: Duration) -> Result<Vec<serde_json::Value>, DatabaseError> {
        let statements: Vec<String> = crate::sql_text::statements(cql).into_iter().filter(|statement| !statement.trim().is_empty()).collect();
        if statements.len() != 1 {
            return Err(DatabaseError::new("Send one statement at a time."));
        }
        let verb = crate::sql_text::statement_verbs(cql).into_iter().next().unwrap_or_default();
        if verb != "SELECT" {
            return Err(DatabaseError::new(format!("Only a SELECT may run here, not {verb}.")));
        }
        let mut rows = Vec::new();
        let read = self.scan(statements[0].trim(), |page| {
            for row in &page.rows {
                let object: serde_json::Map<String, serde_json::Value> =
                    page.names.iter().cloned().zip(row.iter().map(|value| value.as_ref().map_or(serde_json::Value::Null, json))).collect();
                rows.push(serde_json::Value::Object(object));
                if rows.len() as u64 > max_rows {
                    return false;
                }
            }
            true
        });
        match tokio::time::timeout(timeout, read).await {
            Ok(result) => result?,
            Err(_) => return Err(DatabaseError::new(format!("The query ran longer than {} seconds and was stopped.", timeout.as_secs()))),
        }
        Ok(rows)
    }
}

/// Statements a read-only connection may run.
const READ_VERBS: &[&str] = &["SELECT", "USE", "DESCRIBE", "DESC", "LIST"];

/// Through an SSH tunnel, every node the cluster names is reached at the
/// tunnel's local end: the server behind it coordinates for the others.
struct ThroughTunnel(std::net::SocketAddr);

#[async_trait::async_trait]
impl scylla::policies::address_translator::AddressTranslator for ThroughTunnel {
    async fn translate_address(
        &self,
        _peer: &scylla::policies::address_translator::UntranslatedPeer,
    ) -> Result<std::net::SocketAddr, scylla::errors::TranslationError> {
        Ok(self.0)
    }
}

/// rustls with ring. With `verify`, Mozilla's roots check the certificate
/// and the host name; without it any certificate is taken, as `require`
/// does for the other engines.
fn tls_config(verify: bool) -> Result<scylla::client::session::TlsContext, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|error| format!("TLS setup failed: {error}"))?;
    let config = if verify {
        let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
        builder.with_root_certificates(roots).with_no_client_auth()
    } else {
        builder.dangerous().with_custom_certificate_verifier(Arc::new(AnyCertificate(provider))).with_no_client_auth()
    };
    Ok(Arc::new(config).into())
}

#[derive(Debug)]
struct AnyCertificate(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for AnyCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

// MARK: - Values

/// A value as the grid shows it: text without quotes, timestamps in UTC
/// with a Z, bytes as `\x…` hex, collections as cqlsh prints them.
pub(crate) fn text(value: &CqlValue) -> String {
    match value {
        CqlValue::Ascii(text) | CqlValue::Text(text) => text.clone(),
        CqlValue::Blob(bytes) => {
            let mut text = String::from("\\x");
            for byte in bytes {
                text.push_str(&format!("{byte:02x}"));
            }
            text
        }
        CqlValue::Empty => String::new(),
        CqlValue::Decimal(decimal) => {
            let (bytes, scale) = decimal.as_signed_be_bytes_slice_and_exponent();
            match signed_integer(bytes) {
                Some(unscaled) => decimal_text(unscaled, scale),
                None => value.to_string(),
            }
        }
        CqlValue::Varint(varint) => signed_integer(varint.as_signed_bytes_be_slice()).map_or_else(|| value.to_string(), |number| number.to_string()),
        CqlValue::Timestamp(timestamp) => match TryInto::<chrono::DateTime<chrono::Utc>>::try_into(*timestamp) {
            Ok(time) => format!("{}Z", time.naive_utc()),
            Err(_) => timestamp.0.to_string(),
        },
        CqlValue::Date(date) => match TryInto::<chrono::NaiveDate>::try_into(*date) {
            Ok(date) => date.to_string(),
            Err(_) => date.0.to_string(),
        },
        CqlValue::Time(time) => {
            let nanos = time.0;
            let fraction = nanos % 1_000_000_000;
            let base = format!("{:02}:{:02}:{:02}", nanos / 3_600_000_000_000, nanos / 60_000_000_000 % 60, nanos / 1_000_000_000 % 60);
            if fraction == 0 { base } else { format!("{base}.{}", format!("{fraction:09}").trim_end_matches('0')) }
        }
        CqlValue::Inet(address) => address.to_string(),
        CqlValue::Uuid(uuid) => uuid.to_string(),
        CqlValue::Timeuuid(uuid) => uuid.to_string(),
        CqlValue::Counter(counter) => counter.0.to_string(),
        // Numbers, booleans, durations and collections: as CQL writes them.
        other => other.to_string(),
    }
}

/// Big-endian two's complement bytes as a number, when they fit.
fn signed_integer(bytes: &[u8]) -> Option<i128> {
    if bytes.is_empty() || bytes.len() > 16 {
        return None;
    }
    let negative = bytes[0] & 0x80 != 0;
    let mut padded = [if negative { 0xff } else { 0 }; 16];
    padded[16 - bytes.len()..].copy_from_slice(bytes);
    Some(i128::from_be_bytes(padded))
}

/// `unscaled` × 10^-`scale` in plain digits: 950, 2 → "9.50".
fn decimal_text(unscaled: i128, scale: i32) -> String {
    if scale <= 0 {
        return format!("{unscaled}{}", "0".repeat(scale.unsigned_abs() as usize));
    }
    let digits = unscaled.unsigned_abs().to_string();
    let scale = scale as usize;
    let padded = if digits.len() <= scale { format!("{}{digits}", "0".repeat(scale - digits.len() + 1)) } else { digits };
    let (whole, fraction) = padded.split_at(padded.len() - scale);
    format!("{}{whole}.{fraction}", if unscaled < 0 { "-" } else { "" })
}

fn json(value: &CqlValue) -> serde_json::Value {
    match value {
        CqlValue::Boolean(bit) => serde_json::Value::Bool(*bit),
        CqlValue::Int(_)
        | CqlValue::BigInt(_)
        | CqlValue::SmallInt(_)
        | CqlValue::TinyInt(_)
        | CqlValue::Float(_)
        | CqlValue::Double(_)
        | CqlValue::Counter(_)
        | CqlValue::Decimal(_)
        | CqlValue::Varint(_) => {
            let text = text(value);
            text.parse::<serde_json::Number>().map(serde_json::Value::Number).unwrap_or(serde_json::Value::String(text))
        }
        other => serde_json::Value::String(text(other)),
    }
}

fn is_numeric_column(column_type: &ColumnType<'_>) -> bool {
    matches!(
        column_type,
        ColumnType::Native(
            NativeType::Int
                | NativeType::BigInt
                | NativeType::SmallInt
                | NativeType::TinyInt
                | NativeType::Varint
                | NativeType::Decimal
                | NativeType::Float
                | NativeType::Double
                | NativeType::Counter
        )
    )
}

/// The type's name before any `<…>`: "frozen<list<text>>" → "frozen".
fn base_type(column_type: &str) -> String {
    column_type.split('<').next().unwrap_or(column_type).trim().to_ascii_lowercase()
}

pub(crate) fn is_numeric_type(column_type: &str) -> bool {
    matches!(base_type(column_type).as_str(), "int" | "bigint" | "smallint" | "tinyint" | "varint" | "decimal" | "float" | "double" | "counter")
}

/// `text` as a CQL literal of `column_type`. Numbers, UUIDs and booleans
/// are checked, so typed text can't become CQL; collections, tuples and
/// user types are taken as CQL literals, as the grid shows them, after a
/// check that they are one balanced value.
pub(crate) fn literal(text: &str, column_type: &str) -> Result<String, DatabaseError> {
    let invalid = || DatabaseError::new(format!("“{text}” isn't a valid {column_type}."));
    let trimmed = text.trim();
    Ok(match base_type(column_type).as_str() {
        "int" | "bigint" | "smallint" | "tinyint" | "varint" | "counter" => {
            trimmed.parse::<i128>().map_err(|_| invalid())?;
            trimmed.to_string()
        }
        "decimal" | "float" | "double" => {
            if !matches!(trimmed, "NaN" | "Infinity" | "-Infinity") {
                trimmed.parse::<f64>().map_err(|_| invalid())?;
            }
            trimmed.to_string()
        }
        "boolean" => match trimmed.to_ascii_lowercase().as_str() {
            "true" | "1" => "true".into(),
            "false" | "0" => "false".into(),
            _ => return Err(invalid()),
        },
        "uuid" | "timeuuid" => {
            uuid::Uuid::parse_str(trimmed).map_err(|_| invalid())?;
            trimmed.to_string()
        }
        "blob" => {
            let hex = trimmed.strip_prefix("\\x").or_else(|| trimmed.strip_prefix("0x")).ok_or_else(invalid)?;
            if hex.len() % 2 != 0 || !hex.chars().all(|character| character.is_ascii_hexdigit()) {
                return Err(invalid());
            }
            format!("0x{hex}")
        }
        "duration" => {
            if trimmed.is_empty() || !trimmed.chars().all(|character| character.is_ascii_alphanumeric() || character == '-') {
                return Err(invalid());
            }
            trimmed.to_string()
        }
        "list" | "set" | "map" | "tuple" | "frozen" | "vector" => {
            check_literal(trimmed).ok_or_else(invalid)?;
            trimmed.to_string()
        }
        "text" | "varchar" | "ascii" | "inet" | "timestamp" | "date" | "time" => quote_literal(text)?,
        // A user-defined type: a {field: value} literal.
        _ if trimmed.starts_with('{') => {
            check_literal(trimmed).ok_or_else(invalid)?;
            trimmed.to_string()
        }
        _ => quote_literal(text)?,
    })
}

/// One collection literal: brackets balanced outside strings, and no `;`
/// or comment that could end it and start another statement.
fn check_literal(text: &str) -> Option<()> {
    let mut depth = 0i32;
    let mut chars = text.chars().peekable();
    let mut ended = false;
    while let Some(character) = chars.next() {
        if ended && !character.is_whitespace() {
            return None;
        }
        match character {
            '\'' => {
                // '' inside a string is a quote; reading on finds the next one.
                loop {
                    match chars.next()? {
                        '\'' if chars.peek() == Some(&'\'') => {
                            chars.next();
                        }
                        '\'' => break,
                        _ => {}
                    }
                }
            }
            '[' | '{' | '(' => depth += 1,
            ']' | '}' | ')' => {
                depth -= 1;
                if depth < 0 {
                    return None;
                }
                ended = depth == 0;
            }
            ';' | '-' | '/' if depth == 0 => return None,
            ';' => return None,
            '-' if chars.peek() == Some(&'-') => return None,
            '/' if matches!(chars.peek(), Some('*' | '/')) => return None,
            _ => {}
        }
    }
    (depth == 0 && ended).then_some(())
}

// MARK: - Catalog

fn qualified(relation: &Relation) -> String {
    relation.quoted_name()
}

impl DatabaseSession {
    fn cassandra_link(&self) -> Result<CassandraLink, DatabaseError> {
        self.cassandra().ok_or_else(|| DatabaseError::new("This needs a Cassandra connection."))
    }

    pub(crate) async fn cassandra_schemas(&self) -> Result<Vec<SchemaContents>, DatabaseError> {
        let link = self.cassandra_link()?;
        crate::on_runtime(async move {
            let keyspaces = link.text_rows("SELECT keyspace_name FROM system_schema.keyspaces".into()).await?;
            let tables = link.text_rows("SELECT keyspace_name, table_name FROM system_schema.tables".into()).await?;
            let views = link.text_rows("SELECT keyspace_name, view_name FROM system_schema.views".into()).await?;
            let mut schemas: Vec<SchemaContents> = keyspaces
                .into_iter()
                .filter_map(|row| row.into_iter().next().flatten())
                .filter(|name| !SYSTEM_KEYSPACES.contains(&name.as_str()))
                .map(|name| SchemaContents { name, relations: Vec::new() })
                .collect();
            for (rows, kind) in [(tables, RelationKind::Table), (views, RelationKind::View)] {
                for row in rows {
                    let [Some(schema), Some(name)] = <[Option<String>; 2]>::try_from(row).unwrap_or_default() else { continue };
                    if let Some(contents) = schemas.iter_mut().find(|contents| contents.name == schema) {
                        contents.relations.push(Relation { schema, name, kind });
                    }
                }
            }
            schemas.sort_by(|a, b| a.name.cmp(&b.name));
            for schema in &mut schemas {
                schema.relations.sort_by(|a, b| a.name.cmp(&b.name));
            }
            Ok(schemas)
        })
        .await
    }

    /// The columns with their role: (name, type, kind, position, clustering order).
    async fn cassandra_column_rows(&self, relation: &Relation) -> Result<Vec<(String, String, String, i64, String)>, DatabaseError> {
        let link = self.cassandra_link()?;
        let cql = format!(
            "SELECT column_name, type, kind, position, clustering_order FROM system_schema.columns \
             WHERE keyspace_name = {} AND table_name = {}",
            quote_literal(&relation.schema)?,
            quote_literal(&relation.name)?
        );
        let rows = crate::on_runtime(async move { link.text_rows(cql).await }).await?;
        let mut columns: Vec<_> = rows
            .into_iter()
            .filter_map(|row| {
                let [Some(name), Some(column_type), Some(kind), position, order] = <[Option<String>; 5]>::try_from(row).ok()? else {
                    return None;
                };
                Some((name, column_type, kind, position.and_then(|text| text.parse().ok()).unwrap_or(0), order.unwrap_or_default()))
            })
            .collect();
        // As Cassandra lists them: partition key, clustering columns, then the rest by name.
        let rank = |kind: &str| match kind {
            "partition_key" => 0,
            "clustering" => 1,
            "static" => 2,
            _ => 3,
        };
        columns.sort_by(|a, b| rank(&a.2).cmp(&rank(&b.2)).then(a.3.cmp(&b.3)).then(a.0.cmp(&b.0)));
        Ok(columns)
    }

    pub(crate) async fn cassandra_columns(&self, relation: &Relation) -> Result<Vec<Column>, DatabaseError> {
        Ok(self
            .cassandra_column_rows(relation)
            .await?
            .into_iter()
            .map(|(name, column_type, _, _, _)| {
                let kind = match column_type.as_str() {
                    "date" => ColumnKind::Date,
                    "time" => ColumnKind::Time,
                    "timestamp" => ColumnKind::TimestampTz,
                    "boolean" => ColumnKind::Boolean,
                    _ => ColumnKind::Other,
                };
                Column { name, is_numeric: is_numeric_type(&column_type), column_type, kind, enum_values: Vec::new() }
            })
            .collect())
    }

    /// The partition key, then the clustering columns.
    pub(crate) async fn cassandra_primary_key(&self, relation: &Relation) -> Result<Vec<String>, DatabaseError> {
        Ok(self
            .cassandra_column_rows(relation)
            .await?
            .into_iter()
            .filter(|column| column.2 == "partition_key" || column.2 == "clustering")
            .map(|column| column.0)
            .collect())
    }

    pub(crate) fn cassandra_foreign_keys(&self) -> Vec<ForeignKey> {
        Vec::new()
    }

    /// Reads on from the start until `offset` matching rows are passed, then
    /// keeps `limit`. Filters the server can't apply run here.
    pub(crate) async fn cassandra_rows(
        &self,
        relation: Relation,
        columns: Vec<Column>,
        filters: Vec<RowFilter>,
        sort: Option<RowSort>,
        offset: u64,
        limit: u64,
    ) -> Result<RowPage, DatabaseError> {
        if sort.is_some() {
            return Err(DatabaseError::new(
                "Cassandra returns rows in partition order and can't sort a whole table. Use ORDER BY on a clustering column in a query for one partition.",
            ));
        }
        let key = self.cassandra_primary_key(&relation).await?;
        let plan = ScanPlan::new(&relation, &columns, &filters, &key, &self.cassandra_partition_key(&relation).await?)?;
        let link = self.cassandra_link()?;
        crate::on_runtime(async move {
            let mut rows = Vec::new();
            let mut skipped = 0u64;
            link.scan(&plan.cql, |page| {
                for row in &page.rows {
                    let values: Vec<Option<String>> = row.iter().map(|value| value.as_ref().map(text)).collect();
                    if !plan.matches(&values) {
                        continue;
                    }
                    if skipped < offset {
                        skipped += 1;
                        continue;
                    }
                    rows.push(values.into_iter().map(|value| value.map(|text| crate::query::cut(&text))).collect());
                    if rows.len() as u64 >= limit {
                        return false;
                    }
                }
                true
            })
            .await?;
            Ok(RowPage { rows, offset })
        })
        .await
    }

    /// Counts by reading every row: Cassandra keeps no exact count, and its
    /// COUNT(*) times out on a big table.
    pub(crate) async fn cassandra_count(&self, relation: Relation, columns: Vec<Column>, filters: Vec<RowFilter>) -> Result<u64, DatabaseError> {
        let key = self.cassandra_primary_key(&relation).await?;
        let partition = self.cassandra_partition_key(&relation).await?;
        // Without filters only the key is read.
        let read: Vec<Column> = if filters.is_empty() {
            columns.into_iter().filter(|column| key.first() == Some(&column.name)).collect()
        } else {
            columns
        };
        let plan = ScanPlan::new(&relation, &read, &filters, &key, &partition)?;
        let link = self.cassandra_link()?;
        crate::on_runtime(async move {
            let mut count = 0u64;
            link.scan(&plan.cql, |page| {
                count += page.rows.iter().filter(|row| plan.matches(&row.iter().map(|value| value.as_ref().map(text)).collect::<Vec<_>>())).count() as u64;
                true
            })
            .await?;
            Ok(count)
        })
        .await
    }

    async fn cassandra_partition_key(&self, relation: &Relation) -> Result<Vec<String>, DatabaseError> {
        Ok(self.cassandra_column_rows(relation).await?.into_iter().filter(|column| column.2 == "partition_key").map(|column| column.0).collect())
    }

    /// The rows with these keys, whole values as text.
    pub(crate) async fn cassandra_full_rows(
        &self,
        relation: Relation,
        columns: Vec<Column>,
        primary_key: Vec<String>,
        keys: Vec<Vec<String>>,
    ) -> Result<Vec<Vec<Option<String>>>, DatabaseError> {
        let types: Vec<String> = primary_key
            .iter()
            .map(|name| columns.iter().find(|column| &column.name == name).map(|column| column.column_type.clone()).unwrap_or_default())
            .collect();
        let select = columns.iter().map(|column| quote_identifier(&column.name)).collect::<Vec<_>>().join(", ");
        let link = self.cassandra_link()?;
        let mut statements = Vec::new();
        for key in &keys {
            statements.push(format!("SELECT {select} FROM {} WHERE {}", qualified(&relation), key_condition(&primary_key, &types, key)?));
        }
        crate::on_runtime(async move {
            let mut rows = Vec::new();
            for statement in statements {
                rows.extend(link.text_rows(statement).await?.into_iter().next());
            }
            Ok(rows)
        })
        .await
    }

    /// CSV, JSON or CQL INSERTs of the rows that match, read page by page.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn cassandra_export(
        &self,
        relation: Relation,
        columns: Vec<Column>,
        filters: Vec<RowFilter>,
        sort: Option<RowSort>,
        primary_key: Vec<String>,
        scope: crate::export::ExportScope,
        format: crate::export::ExportFormat,
        path: String,
    ) -> Result<u64, DatabaseError> {
        use crate::export::{ExportFormat, ExportScope};
        use std::io::Write;
        let (offset, limit, keys) = match scope {
            ExportScope::All => (0, u64::MAX, None),
            ExportScope::Page { offset, limit } => (offset, limit, None),
            ExportScope::Keys { keys } => (0, u64::MAX, Some(keys)),
        };
        if sort.is_some() {
            return Err(DatabaseError::new("Cassandra can't sort a whole table; export without a sort."));
        }
        let key = self.cassandra_primary_key(&relation).await?;
        let plan = ScanPlan::new(&relation, &columns, &filters, &key, &self.cassandra_partition_key(&relation).await?)?;
        let key_positions: Vec<usize> =
            primary_key.iter().filter_map(|name| columns.iter().position(|column| &column.name == name)).collect();
        let link = self.cassandra_link()?;
        let target = qualified(&relation);
        let names = columns.iter().map(|column| quote_identifier(&column.name)).collect::<Vec<_>>().join(", ");
        crate::on_runtime(async move {
            let write_error = |error: std::io::Error| DatabaseError::new(format!("Can't write {path}: {error}."));
            let file = std::fs::File::create(&path).map_err(|error| DatabaseError::new(format!("Can't create {path}: {error}.")))?;
            let mut out = std::io::BufWriter::new(file);
            match format {
                ExportFormat::Csv => {
                    let header = columns.iter().map(|column| crate::sqlite::csv_field(&column.name)).collect::<Vec<_>>().join(",");
                    writeln!(out, "{header}").map_err(write_error)?;
                }
                ExportFormat::Json => write!(out, "[").map_err(write_error)?,
                ExportFormat::Sql => {}
            }
            let mut count = 0u64;
            let mut skipped = 0u64;
            let mut failure = None;
            link.scan(&plan.cql, |page| {
                for row in &page.rows {
                    let values: Vec<Option<String>> = row.iter().map(|value| value.as_ref().map(text)).collect();
                    if !plan.matches(&values) {
                        continue;
                    }
                    if let Some(keys) = &keys {
                        let row_key: Vec<String> = key_positions.iter().map(|index| values[*index].clone().unwrap_or_default()).collect();
                        if !keys.contains(&row_key) {
                            continue;
                        }
                    }
                    if skipped < offset {
                        skipped += 1;
                        continue;
                    }
                    let line = match format {
                        ExportFormat::Csv => values.iter().map(|value| value.as_deref().map(crate::sqlite::csv_field).unwrap_or_default()).collect::<Vec<_>>().join(",") + "\n",
                        ExportFormat::Json => {
                            let object: serde_json::Map<String, serde_json::Value> = columns
                                .iter()
                                .map(|column| column.name.clone())
                                .zip(row.iter().map(|value| value.as_ref().map_or(serde_json::Value::Null, json)))
                                .collect();
                            format!("{}{}", if count == 0 { "\n  " } else { ",\n  " }, serde_json::Value::Object(object))
                        }
                        ExportFormat::Sql => {
                            // The driver writes each value as a CQL literal.
                            let literals = row.iter().map(|value| value.as_ref().map_or_else(|| "null".to_string(), |value| value.to_string()));
                            format!("INSERT INTO {target} ({names}) VALUES ({});\n", literals.collect::<Vec<_>>().join(", "))
                        }
                    };
                    if let Err(error) = out.write_all(line.as_bytes()) {
                        failure = Some(write_error(error));
                        return false;
                    }
                    count += 1;
                    if count >= limit {
                        return false;
                    }
                }
                true
            })
            .await?;
            if let Some(failure) = failure {
                return Err(failure);
            }
            if format == ExportFormat::Json {
                out.write_all(if count == 0 { b"]\n" } else { b"\n]\n" }).map_err(write_error)?;
            }
            out.flush().map_err(write_error)?;
            Ok(count)
        })
        .await
    }
}

/// The SELECT a page, count or export runs, and the filters left for here.
struct ScanPlan {
    cql: String,
    /// Filters the server can't apply, with their column's position.
    local: Vec<(Option<usize>, FilterOperator, String)>,
}

impl ScanPlan {
    fn new(relation: &Relation, columns: &[Column], filters: &[RowFilter], key: &[String], partition: &[String]) -> Result<Self, DatabaseError> {
        let select = if columns.is_empty() {
            key.first().map(|name| quote_identifier(name)).unwrap_or_else(|| "*".into())
        } else {
            columns.iter().map(|column| quote_identifier(&column.name)).collect::<Vec<_>>().join(", ")
        };
        let mut server = Vec::new();
        let mut local = Vec::new();
        for filter in filters {
            match &filter.target {
                FilterTarget::RawSql => {
                    let condition = filter.value.trim().trim_end_matches(';').trim();
                    if condition.is_empty() {
                        return Err(DatabaseError::new("The CQL filter is empty."));
                    }
                    server.push(condition.to_string());
                }
                FilterTarget::AnyColumn => local.push((None, filter.operator, filter.value.clone())),
                FilterTarget::Column { name } => {
                    let position = columns.iter().position(|column| &column.name == name);
                    let column_type = position.map(|index| columns[index].column_type.as_str()).unwrap_or("text");
                    // The server compares plain values; a partition key only by =.
                    let comparison = match filter.operator {
                        FilterOperator::Equals => Some("="),
                        FilterOperator::GreaterThan => Some(">"),
                        FilterOperator::GreaterOrEqual => Some(">="),
                        FilterOperator::LessThan => Some("<"),
                        FilterOperator::LessOrEqual => Some("<="),
                        _ => None,
                    };
                    let plain = !column_type.contains('<') && base_type(column_type) != "duration";
                    let usable = comparison.is_some_and(|operator| operator == "=" || !partition.contains(name));
                    match (comparison, plain && usable && position.is_some()) {
                        (Some(operator), true) => {
                            server.push(format!("{} {operator} {}", quote_identifier(name), literal(&filter.value, column_type)?))
                        }
                        _ => {
                            let Some(position) = position else {
                                return Err(DatabaseError::new(format!("No column named {name}.")));
                            };
                            local.push((Some(position), filter.operator, filter.value.clone()));
                        }
                    }
                }
            }
        }
        let mut cql = format!("SELECT {select} FROM {}", qualified(relation));
        if !server.is_empty() {
            cql += &format!(" WHERE {} ALLOW FILTERING", server.join(" AND "));
        }
        Ok(Self { cql, local })
    }

    fn matches(&self, values: &[Option<String>]) -> bool {
        self.local.iter().all(|(position, operator, value)| match position {
            Some(index) => compare(values.get(*index).cloned().flatten().as_deref(), *operator, value),
            None => values.iter().any(|cell| cell.is_some() && compare(cell.as_deref(), *operator, value)),
        })
    }
}

/// One filter on one value, as text: case-insensitive matches, and numbers
/// compared as numbers when both sides are numbers.
fn compare(cell: Option<&str>, operator: FilterOperator, value: &str) -> bool {
    let lower = |text: &str| text.to_lowercase();
    let order = |cell: &str| match (cell.parse::<f64>(), value.parse::<f64>()) {
        (Ok(left), Ok(right)) => left.partial_cmp(&right),
        _ => Some(cell.cmp(value)),
    };
    let list = || value.split(',').map(str::trim).filter(|item| !item.is_empty()).map(str::to_string).collect::<Vec<_>>();
    match (operator, cell) {
        (FilterOperator::IsNull, cell) => cell.is_none(),
        (FilterOperator::IsNotNull, cell) => cell.is_some(),
        (FilterOperator::NotEquals, cell) => cell != Some(value),
        (FilterOperator::NotContains, None) => true,
        (FilterOperator::NotIn, None) => true,
        (_, None) => false,
        (FilterOperator::Equals, Some(cell)) => cell == value,
        (FilterOperator::GreaterThan, Some(cell)) => order(cell).is_some_and(|ordering| ordering.is_gt()),
        (FilterOperator::GreaterOrEqual, Some(cell)) => order(cell).is_some_and(|ordering| ordering.is_ge()),
        (FilterOperator::LessThan, Some(cell)) => order(cell).is_some_and(|ordering| ordering.is_lt()),
        (FilterOperator::LessOrEqual, Some(cell)) => order(cell).is_some_and(|ordering| ordering.is_le()),
        (FilterOperator::Contains, Some(cell)) => lower(cell).contains(&lower(value)),
        (FilterOperator::NotContains, Some(cell)) => !lower(cell).contains(&lower(value)),
        (FilterOperator::StartsWith, Some(cell)) => lower(cell).starts_with(&lower(value)),
        (FilterOperator::EndsWith, Some(cell)) => lower(cell).ends_with(&lower(value)),
        (FilterOperator::Like, Some(cell)) => like(cell, value),
        (FilterOperator::In, Some(cell)) => list().iter().any(|item| item == cell),
        (FilterOperator::NotIn, Some(cell)) => !list().iter().any(|item| item == cell),
    }
}

/// SQL's LIKE: `%` any run of characters, `_` any one; case-sensitive.
fn like(text: &str, pattern: &str) -> bool {
    let text: Vec<char> = text.chars().collect();
    let pattern: Vec<char> = pattern.chars().collect();
    let (mut t, mut p) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while t < text.len() {
        if p < pattern.len() && (pattern[p] == '_' || pattern[p] == text[t]) {
            t += 1;
            p += 1;
        } else if p < pattern.len() && pattern[p] == '%' {
            star = Some(p);
            mark = t;
            p += 1;
        } else if let Some(position) = star {
            p = position + 1;
            mark += 1;
            t = mark;
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|character| *character == '%')
}

/// `"a" = 1 AND "b" = 'x'`, each value as its column's literal.
fn key_condition(primary_key: &[String], types: &[String], key: &[String]) -> Result<String, DatabaseError> {
    if primary_key.is_empty() || primary_key.len() != key.len() {
        return Err(DatabaseError::new("The row's primary key doesn't match the table's."));
    }
    Ok(primary_key
        .iter()
        .zip(types)
        .zip(key)
        .map(|((name, column_type), value)| Ok(format!("{} = {}", quote_identifier(name), literal(value, column_type)?)))
        .collect::<Result<Vec<_>, DatabaseError>>()?
        .join(" AND "))
}

// MARK: - Changes

fn draft_cql(draft: Option<&CellDraft>, column_type: &str) -> Result<String, DatabaseError> {
    match draft {
        Some(CellDraft::Value { text }) => literal(text, column_type),
        Some(CellDraft::Null) | None => Ok("null".into()),
        Some(CellDraft::Default) => Err(DatabaseError::new("Cassandra has no column defaults. Type the value instead.")),
        Some(CellDraft::Function { function }) => Ok(match function {
            SqlFunction::Now => "toTimestamp(now())",
            SqlFunction::CurrentDate => "currentDate()",
            SqlFunction::CurrentTime => "currentTime()",
        }
        .into()),
    }
}

/// The statements for the pending changes, as Review shows them: updates,
/// deletes, then inserts. They run in one logged batch.
pub(crate) fn change_statements(
    changes: &PendingChanges,
    relation: &Relation,
    columns: &[Column],
    primary_key: &[String],
) -> Result<Vec<String>, DatabaseError> {
    let column_type = |name: &str| columns.iter().find(|column| column.name == name).map_or("text", |column| column.column_type.as_str());
    let key_types: Vec<String> = primary_key.iter().map(|name| column_type(name).to_string()).collect();
    let table = qualified(relation);
    let mut statements = Vec::new();
    for update in &changes.updates {
        let set: Vec<&Column> = columns.iter().filter(|column| update.values.contains_key(&column.name)).collect();
        if set.is_empty() {
            continue;
        }
        if let Some(column) = set.iter().find(|column| primary_key.contains(&column.name)) {
            return Err(DatabaseError::new(format!(
                "Cassandra can't change {}: it is part of the primary key. Add a row with the new key and delete this one.",
                column.name
            )));
        }
        let assignments = set
            .iter()
            .map(|column| Ok(format!("{} = {}", quote_identifier(&column.name), draft_cql(update.values.get(&column.name), &column.column_type)?)))
            .collect::<Result<Vec<_>, DatabaseError>>()?
            .join(", ");
        statements.push(format!("UPDATE {table} SET {assignments} WHERE {}", key_condition(primary_key, &key_types, &update.key)?));
    }
    for key in &changes.deletes {
        statements.push(format!("DELETE FROM {table} WHERE {}", key_condition(primary_key, &key_types, key)?));
    }
    for row in &changes.inserts {
        let set: Vec<&Column> = columns.iter().filter(|column| row.values.contains_key(&column.name)).collect();
        if let Some(missing) = primary_key.iter().find(|name| !set.iter().any(|column| &&column.name == name)) {
            return Err(DatabaseError::new(format!("A new row needs a value for {missing}, which is part of the primary key.")));
        }
        let names = set.iter().map(|column| quote_identifier(&column.name)).collect::<Vec<_>>().join(", ");
        let values =
            set.iter().map(|column| draft_cql(row.values.get(&column.name), &column.column_type)).collect::<Result<Vec<_>, _>>()?.join(", ");
        statements.push(format!("INSERT INTO {table} ({names}) VALUES ({values})"));
    }
    Ok(statements)
}

impl DatabaseSession {
    /// Applies the changes in one logged batch, which Cassandra applies
    /// whole. Updates and inserts would write a row whether or not it is
    /// there, so each row is looked up first: an edited or deleted row must
    /// still exist, and a new row's key must be free.
    pub(crate) async fn cassandra_commit(
        &self,
        changes: PendingChanges,
        relation: Relation,
        columns: Vec<Column>,
        primary_key: Vec<String>,
    ) -> Result<(), DatabaseError> {
        let statements = change_statements(&changes, &relation, &columns, &primary_key)?;
        if statements.is_empty() {
            return Ok(());
        }
        let key_types: Vec<String> = primary_key
            .iter()
            .map(|name| columns.iter().find(|column| &column.name == name).map(|column| column.column_type.clone()).unwrap_or_default())
            .collect();
        let probe = |key: &[String]| -> Result<String, DatabaseError> {
            Ok(format!(
                "SELECT {} FROM {} WHERE {} LIMIT 1",
                quote_identifier(&primary_key[0]),
                qualified(&relation),
                key_condition(&primary_key, &key_types, key)?
            ))
        };
        let mut must_exist = Vec::new();
        for key in changes.updates.iter().map(|update| &update.key).chain(changes.deletes.iter()) {
            must_exist.push(probe(key)?);
        }
        let mut must_be_free = Vec::new();
        for row in &changes.inserts {
            let key: Vec<String> = primary_key
                .iter()
                .map(|name| match row.values.get(name) {
                    Some(CellDraft::Value { text }) => text.clone(),
                    _ => String::new(),
                })
                .collect();
            must_be_free.push((probe(&key)?, key.join(", ")));
        }
        let link = self.cassandra_link()?;
        link.refuse_writes()?;
        crate::on_runtime(async move {
            for cql in must_exist {
                if link.text_rows(cql).await?.is_empty() {
                    return Err(DatabaseError::new("A row was changed or deleted by someone else. Nothing was saved; refresh and try again."));
                }
            }
            for (cql, key) in must_be_free {
                if !link.text_rows(cql).await?.is_empty() {
                    return Err(DatabaseError::new(format!("A row with the key {key} already exists. Nothing was saved.")));
                }
            }
            let batch = format!("BEGIN BATCH\n{};\nAPPLY BATCH", statements.join(";\n"));
            link.session.query_unpaged(batch, &[]).await.map_err(|error| link.fail(error))?;
            Ok(())
        })
        .await
    }
}

// MARK: - Structure

impl DatabaseSession {
    pub(crate) async fn cassandra_structure(&self, relation: &Relation) -> Result<TableStructure, DatabaseError> {
        let column_rows = self.cassandra_column_rows(relation).await?;
        let columns = column_rows
            .iter()
            .map(|(name, column_type, kind, _, order)| {
                // The column's part in the key, where SQL engines show a comment.
                let role = match kind.as_str() {
                    "partition_key" => Some("partition key".to_string()),
                    "clustering" => Some(format!("clustering key, {}", if order.eq_ignore_ascii_case("desc") { "descending" } else { "ascending" })),
                    "static" => Some("static".to_string()),
                    _ => None,
                };
                let is_key = kind == "partition_key" || kind == "clustering";
                ColumnDetail { name: name.clone(), column_type: column_type.clone(), nullable: !is_key, default_value: None, comment: role }
            })
            .collect();
        let partition: Vec<String> =
            column_rows.iter().filter(|column| column.2 == "partition_key").map(|column| quote_identifier(&column.0)).collect();
        let clustering: Vec<String> = column_rows.iter().filter(|column| column.2 == "clustering").map(|column| quote_identifier(&column.0)).collect();
        let key = std::iter::once(format!("({})", partition.join(", "))).chain(clustering).collect::<Vec<_>>().join(", ");
        let primary = format!("PRIMARY KEY ({key})");
        let link = self.cassandra_link()?;
        let cql = format!(
            "SELECT index_name, options FROM system_schema.indexes WHERE keyspace_name = {} AND table_name = {}",
            quote_literal(&relation.schema)?,
            quote_literal(&relation.name)?
        );
        let index_rows = crate::on_runtime(async move { link.text_rows(cql).await }).await?;
        let mut indexes = vec![IndexDetail { name: "PRIMARY KEY".into(), definition: primary.clone(), is_unique: true, is_primary: true }];
        for row in index_rows {
            let [Some(name), options] = <[Option<String>; 2]>::try_from(row).unwrap_or_default() else { continue };
            let options = options.unwrap_or_default();
            let target = map_entry(&options, "target").unwrap_or_default();
            let using = if map_entry(&options, "class_name").is_some_and(|class| class.contains("StorageAttached")) { " USING 'sai'" } else { "" };
            indexes.push(IndexDetail {
                definition: format!("CREATE INDEX {} ON {} ({target}){using}", quote_identifier(&name), qualified(relation)),
                name,
                is_unique: false,
                is_primary: false,
            });
        }
        let constraints = vec![ConstraintDetail { name: "PRIMARY KEY".into(), kind: ConstraintKind::PrimaryKey, definition: primary }];
        Ok(TableStructure { columns, indexes, constraints })
    }

    pub(crate) async fn cassandra_apply_structure(&self, relation: Relation, change: StructureChange) -> Result<(), DatabaseError> {
        let key = self.cassandra_primary_key(&relation).await?;
        let statements = structure_change_cql(&relation, &change, &key)?;
        let link = self.cassandra_link()?;
        crate::on_runtime(async move {
            for statement in statements {
                link.execute(statement).await?;
            }
            Ok(())
        })
        .await
    }

    pub(crate) async fn cassandra_structure_change_cql(&self, relation: &Relation, change: StructureChange) -> Result<Vec<String>, DatabaseError> {
        let key = self.cassandra_primary_key(relation).await?;
        structure_change_cql(relation, &change, &key)
    }
}

/// `'value'` of `key` in a map as cqlsh prints it: {'target': 'name'}.
fn map_entry(map: &str, key: &str) -> Option<String> {
    let label = format!("'{key}':");
    let rest = map[map.find(&label)? + label.len()..].trim_start().strip_prefix('\'')?;
    Some(rest[..rest.find('\'')?].to_string())
}

/// The statements for a change. `key` is the table's primary key, whose
/// columns are the only ones Cassandra can rename.
pub(crate) fn structure_change_cql(relation: &Relation, change: &StructureChange, key: &[String]) -> Result<Vec<String>, DatabaseError> {
    let table = format!("ALTER TABLE {}", qualified(relation));
    let name = |name: &str| -> Result<String, DatabaseError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(DatabaseError::new("The name is empty."));
        }
        Ok(quote_identifier(name))
    };
    Ok(match change {
        StructureChange::AddColumn { column } => {
            if non_empty(&column.default_value).is_some() {
                return Err(DatabaseError::new("Cassandra has no column defaults."));
            }
            if non_empty(&column.comment).is_some() {
                return Err(DatabaseError::new("Cassandra has no column comments."));
            }
            let column_type = column.column_type.trim();
            if column_type.is_empty() {
                return Err(DatabaseError::new("The column needs a type."));
            }
            vec![format!("{table} ADD {} {column_type};", name(&column.name)?)]
        }
        StructureChange::EditColumn { original, column } => {
            let new_name = column.name.trim();
            if column.column_type.trim() != original.column_type || non_empty(&column.default_value).is_some() {
                return Err(DatabaseError::new("Cassandra can't change a column's type, and has no defaults."));
            }
            if new_name == original.name {
                return Ok(Vec::new());
            }
            if !key.contains(&original.name) {
                return Err(DatabaseError::new("Cassandra can rename only primary key columns."));
            }
            vec![format!("{table} RENAME {} TO {};", quote_identifier(&original.name), name(new_name)?)]
        }
        StructureChange::DropColumn { name: column, .. } => {
            if key.contains(column) {
                return Err(DatabaseError::new("A primary key column can't be dropped."));
            }
            vec![format!("{table} DROP {};", name(column)?)]
        }
        StructureChange::AddIndex { name: index, columns, unique } => {
            if *unique {
                return Err(DatabaseError::new("Cassandra has no unique indexes."));
            }
            let [column] = columns.as_slice() else {
                return Err(DatabaseError::new("A Cassandra index covers one column."));
            };
            vec![format!("CREATE INDEX {} ON {} ({});", name(index)?, qualified(relation), name(column)?)]
        }
        StructureChange::DropIndex { name: index, .. } => {
            if index == "PRIMARY KEY" {
                return Err(DatabaseError::new("The primary key can't be dropped."));
            }
            vec![format!("DROP INDEX {}.{};", quote_identifier(&relation.schema), name(index)?)]
        }
        StructureChange::AddForeignKey { .. } => return Err(DatabaseError::new("Cassandra has no foreign keys.")),
        StructureChange::DropConstraint { .. } => return Err(DatabaseError::new("The primary key is the only constraint, and can't be dropped.")),
    })
}

/// Cassandra's built-in functions, for autocomplete.
pub(crate) const FUNCTIONS: &[&str] = &[
    "abs", "avg", "blobasbigint", "blobastext", "bigintasblob", "cast", "count", "currentdate", "currenttime", "currenttimestamp",
    "currenttimeuuid", "dateof", "exp", "floor", "fromjson", "log", "log10", "max", "maxtimeuuid", "min", "mintimeuuid", "now",
    "round", "sum", "textasblob", "todate", "tojson", "totimestamp", "token", "ttl", "tounixtimestamp", "uuid", "writetime",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_literals_by_type() {
        assert_eq!(literal("42", "int").unwrap(), "42");
        assert!(literal("42; DROP TABLE x", "int").is_err());
        assert_eq!(literal("it's", "text").unwrap(), "'it''s'");
        assert_eq!(literal("TRUE", "boolean").unwrap(), "true");
        assert_eq!(literal("\\xcafe", "blob").unwrap(), "0xcafe");
        assert_eq!(literal("['a', 'b''c']", "list<text>").unwrap(), "['a', 'b''c']");
        assert_eq!(literal("{'k': 1}", "frozen<map<text, int>>").unwrap(), "{'k': 1}");
        assert!(literal("[1]; DELETE FROM t", "list<int>").is_err());
        assert!(literal("[1] -- x", "list<int>").is_err());
        assert!(literal("[1", "list<int>").is_err());
        assert!(literal("not-a-uuid", "uuid").is_err());
        assert_eq!(literal("2024-01-02 03:04:05Z", "timestamp").unwrap(), "'2024-01-02 03:04:05Z'");
    }

    #[test]
    fn writes_decimals_and_matches_like() {
        assert_eq!(decimal_text(950, 2), "9.50");
        assert_eq!(decimal_text(-5, 3), "-0.005");
        assert_eq!(decimal_text(7, 0), "7");
        assert_eq!(decimal_text(7, -2), "700");
        assert_eq!(signed_integer(&[0xff, 0x38]), Some(-200));
        assert!(like("Grace Hopper", "Gr%er"));
        assert!(like("abc", "a_c"));
        assert!(!like("abc", "a_"));
        assert_eq!(map_entry("{'class_name': 'org.x.StorageAttachedIndex', 'target': 'email'}", "target").as_deref(), Some("email"));
        assert_eq!(map_entry("{'target':'name'}", "target").as_deref(), Some("name"));
    }

    #[test]
    fn compares_filters_here() {
        assert!(compare(Some("Ada"), FilterOperator::Contains, "a"));
        assert!(compare(None, FilterOperator::NotEquals, "7"));
        assert!(compare(Some("10"), FilterOperator::GreaterThan, "9"));
        assert!(compare(Some("b"), FilterOperator::In, "a, b"));
        assert!(compare(None, FilterOperator::IsNull, ""));
    }
}
