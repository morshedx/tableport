//! MySQL and MariaDB. One session holds two connections: TablePort's own
//! SQL runs on one in ANSI_QUOTES and NO_BACKSLASH_ESCAPES mode, so the
//! shared code quotes names with `"` and values with `'` as for the other
//! engines, and a backslash in a value can't end its string early. The
//! user's SQL from the query editor runs on the other, in the server's own
//! mode, where `"` may quote a string as MySQL users expect.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Opts, OptsBuilder, SslOpts, Value};
use tokio::sync::Mutex;

use crate::rows::{quote_literal, Column, ColumnKind};
use crate::schema::{ForeignKey, Relation, RelationKind, SchemaContents};
use crate::session::{ConnectionSettings, TlsMode};
use crate::ssh::{open_tunnel, Tunnel};
use crate::{DatabaseError, DatabaseSession};

/// Schemas the server keeps for itself; hidden from the sidebar.
const SYSTEM_SCHEMAS: &str = "'mysql', 'information_schema', 'performance_schema', 'sys'";

#[derive(Clone)]
pub(crate) struct MysqlLink {
    internal: Arc<Mutex<Conn>>,
    editor: Arc<Mutex<Conn>>,
    /// The editor connection's id, for KILL QUERY (Cancel).
    editor_id: u32,
    /// For the separate connection that sends KILL QUERY.
    opts: Opts,
    /// "MySQL 8.4.13" or "MariaDB 11.4.13".
    pub(crate) version: String,
    pub(crate) is_mariadb: bool,
    closed: Arc<AtomicBool>,
    endpoint: String,
    /// Kept open as long as the connections.
    _tunnel: Option<Arc<Tunnel>>,
}

impl MysqlLink {
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
        let database = Some(settings.database.trim()).filter(|name| !name.is_empty());
        let mut setup = Vec::new();
        if settings.read_only {
            // Every later transaction, and each statement on its own, refuses to write.
            setup.push("SET SESSION TRANSACTION READ ONLY".to_string());
        }
        let base = OptsBuilder::default()
            .ip_or_hostname(host)
            .tcp_port(port)
            .user(Some(settings.user.clone()))
            .pass(settings.password.clone())
            .db_name(database)
            .prefer_socket(false)
            // Probes an idle connection, so one that died without a goodbye is noticed.
            .tcp_keepalive(Some(Duration::from_secs(30)))
            // UPDATE reports the rows it matched, not only those it changed:
            // the edit check counts a row set to its own value as found.
            .client_found_rows(true)
            .connect_attribute("program_name", "tableport");
        let tls = |use_tls: bool| -> Option<SslOpts> {
            if !use_tls {
                return None;
            }
            let verify = settings.tls == TlsMode::VerifyFull;
            let mut ssl = SslOpts::default().with_danger_accept_invalid_certs(!verify).with_danger_skip_domain_validation(!verify);
            // Through SSH the connection goes to 127.0.0.1; the certificate is
            // still checked against the real server name.
            if verify && tunnel.is_some() {
                ssl = ssl.with_danger_tls_hostname_override(Some(settings.host.clone()));
            }
            Some(ssl)
        };
        let internal_setup = setup
            .iter()
            .cloned()
            .chain(std::iter::once(
                "SET SESSION sql_mode = CONCAT_WS(',', NULLIF(@@SESSION.sql_mode, ''), 'ANSI_QUOTES', 'NO_BACKSLASH_ESCAPES')".to_string(),
            ))
            .collect::<Vec<_>>();
        let connect = |use_tls: bool| {
            let internal = Opts::from(base.clone().ssl_opts(tls(use_tls)).setup(internal_setup.clone()));
            let editor = Opts::from(base.clone().ssl_opts(tls(use_tls)).setup(setup.clone()));
            let endpoint = endpoint.clone();
            async move {
                let both = async { tokio::try_join!(Conn::new(internal.clone()), Conn::new(editor)) };
                match tokio::time::timeout(Duration::from_secs(10), both).await {
                    Ok(Ok((internal_conn, editor_conn))) => Ok((internal_conn, editor_conn, internal)),
                    Ok(Err(error)) => Err(error),
                    Err(_) => Err(mysql_async::Error::Other(format!("{endpoint} did not answer within 10 seconds.").into())),
                }
            }
        };
        let connected = match settings.tls {
            TlsMode::Disable => connect(false).await,
            TlsMode::Require | TlsMode::VerifyFull => connect(true).await,
            // TLS when the server has it, as libpq's "prefer".
            TlsMode::Prefer => match connect(true).await {
                Err(mysql_async::Error::Driver(mysql_async::DriverError::NoClientSslFlagFromServer)) => connect(false).await,
                other => other,
            },
        };
        let (mut internal, editor, opts) = connected.map_err(|error| DatabaseError::from_mysql(error, &endpoint))?;
        let version: String = internal
            .query_first("SELECT VERSION()")
            .await
            .map_err(|error| DatabaseError::from_mysql(error, &endpoint))?
            .unwrap_or_default();
        let is_mariadb = version.contains("MariaDB");
        let number = version.split('-').next().unwrap_or(&version).to_string();
        Ok(Self {
            editor_id: editor.id(),
            internal: Arc::new(Mutex::new(internal)),
            editor: Arc::new(Mutex::new(editor)),
            opts,
            version: format!("{} {number}", if is_mariadb { "MariaDB" } else { "MySQL" }),
            is_mariadb,
            closed: Arc::new(AtomicBool::new(false)),
            endpoint,
            _tunnel: tunnel,
        })
    }

    /// The error to show; a broken connection marks the link closed, so the
    /// app offers Reconnect.
    fn fail(&self, error: mysql_async::Error) -> DatabaseError {
        if error.is_fatal() {
            self.closed.store(true, Ordering::Relaxed);
        }
        DatabaseError::from_mysql(error, &self.endpoint)
    }

    async fn internal(&self) -> Result<tokio::sync::MutexGuard<'_, Conn>, DatabaseError> {
        if self.is_closed() {
            return Err(DatabaseError::new(format!("The connection to {} is closed.", self.endpoint)));
        }
        Ok(self.internal.lock().await)
    }

    /// The first result's rows, every value as text (None for NULL).
    pub(crate) async fn text_rows(&self, sql: String) -> Result<Vec<Vec<Option<String>>>, DatabaseError> {
        let mut conn = self.internal().await?;
        let mut result = conn.query_iter(sql).await.map_err(|error| self.fail(error))?;
        let rows: Vec<mysql_async::Row> = result.collect().await.map_err(|error| self.fail(error))?;
        result.drop_result().await.map_err(|error| self.fail(error))?;
        Ok(rows.into_iter().map(|row| row.unwrap().into_iter().map(text).collect()).collect())
    }

    /// Runs SQL and discards any rows; several statements may be separated by `;`.
    pub(crate) async fn execute(&self, sql: String) -> Result<(), DatabaseError> {
        let mut conn = self.internal().await?;
        conn.query_drop(sql).await.map_err(|error| self.fail(error))
    }

    pub(crate) async fn ping(&self) -> Result<(), DatabaseError> {
        let mut conn = self.internal().await?;
        match tokio::time::timeout(Duration::from_secs(5), conn.ping()).await {
            Ok(result) => result.map_err(|error| self.fail(error)),
            Err(_) => Err(DatabaseError::new(format!("{} did not answer within 5 seconds.", self.endpoint))),
        }
    }

    /// Stops the editor's running statement from a separate connection.
    pub(crate) async fn cancel(&self) -> Result<(), DatabaseError> {
        let mut conn = Conn::new(self.opts.clone()).await.map_err(|error| DatabaseError::from_mysql(error, &self.endpoint))?;
        let result = conn.query_drop(format!("KILL QUERY {}", self.editor_id)).await;
        conn.disconnect().await.ok();
        result.map_err(|error| DatabaseError::from_mysql(error, &self.endpoint))
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
    }

    #[allow(dead_code)]
    pub(crate) async fn editor(&self) -> Result<tokio::sync::MutexGuard<'_, Conn>, DatabaseError> {
        if self.is_closed() {
            return Err(DatabaseError::new(format!("The connection to {} is closed.", self.endpoint)));
        }
        Ok(self.editor.lock().await)
    }
}

/// A value of the text protocol as text: every value arrives as bytes.
pub(crate) fn text(value: Value) -> Option<String> {
    match value {
        Value::NULL => None,
        Value::Bytes(bytes) => Some(String::from_utf8(bytes).unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned())),
        Value::Int(number) => Some(number.to_string()),
        Value::UInt(number) => Some(number.to_string()),
        Value::Float(number) => Some(number.to_string()),
        Value::Double(number) => Some(number.to_string()),
        other => Some(other.as_sql(true).trim_matches('\'').to_string()),
    }
}

/// Integer, decimal and floating point types: the grid right-aligns them.
pub(crate) fn is_numeric_type(data_type: &str) -> bool {
    matches!(
        data_type.to_ascii_lowercase().as_str(),
        "tinyint" | "smallint" | "mediumint" | "int" | "integer" | "bigint" | "decimal" | "numeric" | "float" | "double" | "real"
    )
}

/// The labels of `enum('a','b')` or `set(…)`, with doubled quotes undone.
fn enum_labels(column_type: &str) -> Vec<String> {
    let Some(inner) = column_type.find('(').and_then(|open| column_type.rfind(')').map(|close| &column_type[open + 1..close])) else {
        return Vec::new();
    };
    let mut labels = Vec::new();
    let mut chars = inner.chars().peekable();
    while let Some(next) = chars.next() {
        if next != '\'' {
            continue;
        }
        let mut label = String::new();
        while let Some(character) = chars.next() {
            if character == '\'' {
                if chars.peek() == Some(&'\'') {
                    chars.next();
                    label.push('\'');
                } else {
                    break;
                }
            } else {
                label.push(character);
            }
        }
        labels.push(label);
    }
    labels
}

impl DatabaseSession {
    pub(crate) async fn mysql_schemas(&self) -> Result<Vec<SchemaContents>, DatabaseError> {
        let rows = self
            .text_rows(format!(
                "SELECT s.SCHEMA_NAME, t.TABLE_NAME, t.TABLE_TYPE FROM information_schema.SCHEMATA s \
                 LEFT JOIN information_schema.TABLES t ON t.TABLE_SCHEMA = s.SCHEMA_NAME AND t.TABLE_TYPE IN ('BASE TABLE', 'VIEW') \
                 WHERE s.SCHEMA_NAME NOT IN ({SYSTEM_SCHEMAS}) ORDER BY s.SCHEMA_NAME, t.TABLE_NAME"
            ))
            .await?;
        let mut schemas: Vec<SchemaContents> = Vec::new();
        for row in rows {
            let [Some(schema), name, kind] = <[Option<String>; 3]>::try_from(row).unwrap_or_default() else {
                continue;
            };
            if schemas.last().is_none_or(|last| last.name != schema) {
                schemas.push(SchemaContents { name: schema.clone(), relations: Vec::new() });
            }
            let (Some(name), Some(kind), Some(contents)) = (name, kind, schemas.last_mut()) else { continue };
            let kind = if kind == "VIEW" { RelationKind::View } else { RelationKind::Table };
            contents.relations.push(Relation { schema, name, kind });
        }
        Ok(schemas)
    }

    pub(crate) async fn mysql_columns(&self, relation: &Relation) -> Result<Vec<Column>, DatabaseError> {
        let rows = self
            .text_rows(format!(
                "SELECT COLUMN_NAME, COLUMN_TYPE, DATA_TYPE FROM information_schema.COLUMNS \
                 WHERE TABLE_SCHEMA = {} AND TABLE_NAME = {} ORDER BY ORDINAL_POSITION",
                quote_literal(&relation.schema)?,
                quote_literal(&relation.name)?
            ))
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|row| {
                let [Some(name), Some(column_type), Some(data_type)] = <[Option<String>; 3]>::try_from(row).ok()? else {
                    return None;
                };
                let data_type = data_type.to_ascii_lowercase();
                let kind = match data_type.as_str() {
                    "date" => ColumnKind::Date,
                    "time" => ColumnKind::Time,
                    "datetime" | "timestamp" => ColumnKind::Timestamp,
                    "enum" => ColumnKind::Enum,
                    _ => ColumnKind::Other,
                };
                let enum_values = if kind == ColumnKind::Enum { enum_labels(&column_type) } else { Vec::new() };
                Some(Column { name, is_numeric: is_numeric_type(&data_type), column_type, kind, enum_values })
            })
            .collect())
    }

    pub(crate) async fn mysql_primary_key(&self, relation: &Relation) -> Result<Vec<String>, DatabaseError> {
        let rows = self
            .text_rows(format!(
                "SELECT COLUMN_NAME FROM information_schema.KEY_COLUMN_USAGE \
                 WHERE TABLE_SCHEMA = {} AND TABLE_NAME = {} AND CONSTRAINT_NAME = 'PRIMARY' ORDER BY ORDINAL_POSITION",
                quote_literal(&relation.schema)?,
                quote_literal(&relation.name)?
            ))
            .await?;
        Ok(rows.into_iter().filter_map(|row| row.into_iter().next().flatten()).collect())
    }

    /// Single-column foreign keys, which the grid follows with its arrows.
    pub(crate) async fn mysql_foreign_keys(&self, relation: &Relation) -> Result<Vec<ForeignKey>, DatabaseError> {
        let rows = self
            .text_rows(format!(
                "SELECT k.COLUMN_NAME, k.REFERENCED_TABLE_SCHEMA, k.REFERENCED_TABLE_NAME, k.REFERENCED_COLUMN_NAME \
                 FROM information_schema.KEY_COLUMN_USAGE k \
                 WHERE k.TABLE_SCHEMA = {schema} AND k.TABLE_NAME = {table} AND k.REFERENCED_TABLE_NAME IS NOT NULL \
                   AND (SELECT count(*) FROM information_schema.KEY_COLUMN_USAGE other \
                        WHERE other.CONSTRAINT_SCHEMA = k.CONSTRAINT_SCHEMA AND other.TABLE_NAME = k.TABLE_NAME \
                          AND other.CONSTRAINT_NAME = k.CONSTRAINT_NAME) = 1 \
                 ORDER BY k.CONSTRAINT_NAME",
                schema = quote_literal(&relation.schema)?,
                table = quote_literal(&relation.name)?
            ))
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|row| {
                let [Some(column), Some(schema), Some(name), Some(referenced_column)] = <[Option<String>; 4]>::try_from(row).ok()? else {
                    return None;
                };
                Some(ForeignKey { column, referenced_relation: Relation { schema, name, kind: RelationKind::Table }, referenced_column })
            })
            .collect())
    }

    /// The storage engine's estimate, kept in the catalog; None for views.
    pub(crate) async fn mysql_estimated_row_count(&self, relation: &Relation) -> Result<Option<u64>, DatabaseError> {
        let rows = self
            .text_rows(format!(
                "SELECT TABLE_ROWS FROM information_schema.TABLES WHERE TABLE_SCHEMA = {} AND TABLE_NAME = {} AND TABLE_TYPE = 'BASE TABLE'",
                quote_literal(&relation.schema)?,
                quote_literal(&relation.name)?
            ))
            .await?;
        Ok(rows.first().and_then(|row| row.first()).and_then(|cell| cell.as_deref()).and_then(|text| text.parse().ok()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_enum_labels() {
        assert_eq!(enum_labels("enum('small','it''s big','a,b')"), vec!["small", "it's big", "a,b"]);
        assert!(enum_labels("int").is_empty());
    }
}
