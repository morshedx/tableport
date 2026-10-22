//! MongoDB: documents, not rows. The app's screens still fit: databases show
//! as schemas, collections and views as tables, and the grid's columns are
//! the top-level fields found in a sample of documents, each with the BSON
//! type most of them hold. Nested values show as JSON.
//!
//! Edits keep a field's type: the grid's text goes back as a number for a
//! number field, an ObjectId for an ObjectId field, and so on.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::TryStreamExt;
use mongodb::bson::{doc, oid::ObjectId, Bson, Document};
use mongodb::options::{ClientOptions, ServerAddress, Tls, TlsOptions};
use mongodb::Client;

use crate::changes::{CellDraft, PendingChanges};
use crate::rows::{Column, ColumnKind, FilterOperator, FilterTarget, RowFilter, RowPage, RowSort, GRID_CELL_LIMIT};
use crate::schema::{Relation, RelationKind, SchemaContents};
use crate::session::{ConnectionSettings, TlsMode};
use crate::ssh::{open_tunnel, Tunnel};
use crate::{on_runtime, DatabaseError, DatabaseSession};

/// Databases the server keeps for itself.
const SYSTEM_DATABASES: &[&str] = &["admin", "config", "local"];
/// Documents sampled for a collection's fields.
const SAMPLE_SIZE: i64 = 200;

#[derive(Clone)]
pub(crate) struct MongoLink {
    client: Client,
    /// "MongoDB 8.2.12".
    pub(crate) version: String,
    /// Transactions need a replica set or a sharded cluster.
    transactions: bool,
    read_only: bool,
    endpoint: String,
    closed: Arc<AtomicBool>,
    _tunnel: Option<Arc<Tunnel>>,
}

impl MongoLink {
    pub(crate) async fn open(settings: &ConnectionSettings) -> Result<Self, DatabaseError> {
        let endpoint = settings.endpoint();
        let tunnel = match &settings.ssh {
            Some(ssh) if !settings.host.contains("://") => Some(Arc::new(open_tunnel(ssh, &settings.host, settings.port).await?)),
            Some(_) => return Err(DatabaseError::new("An SSH tunnel needs a host and port, not a connection string.")),
            None => None,
        };
        // A connection string (mongodb://, mongodb+srv:// for Atlas) as the
        // host; the password stays in the Keychain, never in the string.
        let mut options = if settings.host.contains("://") {
            ClientOptions::parse(settings.host.trim()).await.map_err(|error| mongo_error(&endpoint, error))?
        } else {
            let (host, port) = match &tunnel {
                Some(tunnel) => ("127.0.0.1".to_string(), tunnel.local_port),
                None => (settings.host.clone(), settings.port),
            };
            let mut options = ClientOptions::default();
            options.hosts = vec![ServerAddress::Tcp { host, port: Some(port) }];
            // Through a tunnel there is one server, whatever the replica set says.
            options.direct_connection = Some(true);
            options
        };
        let user = Some(settings.user.trim()).filter(|user| !user.is_empty()).map(str::to_string);
        let password = settings.password.clone().filter(|password| !password.is_empty());
        if user.is_some() || password.is_some() {
            let mut credential = options.credential.take().unwrap_or_default();
            if user.is_some() {
                credential.username = user;
            }
            if password.is_some() {
                credential.password = password;
            }
            options.credential = Some(credential);
        }
        match settings.tls {
            // A connection string's own tls= setting decides.
            TlsMode::Prefer if settings.host.contains("://") => {}
            TlsMode::Disable | TlsMode::Prefer => options.tls = Some(Tls::Disabled),
            TlsMode::Require => {
                options.tls = Some(Tls::Enabled(
                    // "require": TLS, with neither the certificate nor the name checked.
                    TlsOptions::builder().allow_invalid_certificates(true).build(),
                ))
            }
            TlsMode::VerifyFull => options.tls = Some(Tls::Enabled(TlsOptions::default())),
        }
        options.app_name = Some("tableport".into());
        options.server_selection_timeout = Some(Duration::from_secs(10));
        options.connect_timeout = Some(Duration::from_secs(10));
        let client = Client::with_options(options).map_err(|error| mongo_error(&endpoint, error))?;
        let admin = client.database("admin");
        let build = admin.run_command(doc! { "buildInfo": 1 }).await.map_err(|error| mongo_error(&endpoint, error))?;
        let hello = admin.run_command(doc! { "hello": 1 }).await.map_err(|error| mongo_error(&endpoint, error))?;
        let transactions = hello.get_str("setName").is_ok() || hello.get_str("msg").is_ok_and(|msg| msg == "isdbgrid");
        Ok(Self {
            version: format!("MongoDB {}", build.get_str("version").unwrap_or_default()),
            client,
            transactions,
            read_only: settings.read_only,
            endpoint,
            closed: Arc::new(AtomicBool::new(false)),
            _tunnel: tunnel,
        })
    }

    fn fail(&self, error: mongodb::error::Error) -> DatabaseError {
        if matches!(*error.kind, mongodb::error::ErrorKind::Io(_) | mongodb::error::ErrorKind::ServerSelection { .. }) {
            self.closed.store(true, Ordering::Relaxed);
        }
        mongo_error(&self.endpoint, error)
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
    }

    pub(crate) async fn ping(&self) -> Result<(), DatabaseError> {
        let admin = self.client.database("admin");
        let ping = admin.run_command(doc! { "ping": 1 });
        match tokio::time::timeout(Duration::from_secs(5), ping).await {
            Ok(result) => {
                result.map_err(|error| self.fail(error))?;
                self.closed.store(false, Ordering::Relaxed);
                Ok(())
            }
            Err(_) => Err(DatabaseError::new(format!("{} did not answer within 5 seconds.", self.endpoint))),
        }
    }

    fn collection(&self, relation: &Relation) -> mongodb::Collection<Document> {
        self.client.database(&relation.schema).collection(&relation.name)
    }

    async fn schemas(&self) -> Result<Vec<SchemaContents>, DatabaseError> {
        let mut names = self.client.list_database_names().await.map_err(|error| self.fail(error))?;
        names.retain(|name| !SYSTEM_DATABASES.contains(&name.as_str()));
        names.sort();
        let mut schemas = Vec::new();
        for name in names {
            let specs: Vec<_> = self
                .client
                .database(&name)
                .list_collections()
                .await
                .map_err(|error| self.fail(error))?
                .try_collect()
                .await
                .map_err(|error| self.fail(error))?;
            let mut relations: Vec<Relation> = specs
                .into_iter()
                .filter(|spec| !spec.name.starts_with("system."))
                .map(|spec| Relation {
                    kind: if spec.collection_type == mongodb::results::CollectionType::View { RelationKind::View } else { RelationKind::Table },
                    schema: name.clone(),
                    name: spec.name,
                })
                .collect();
            relations.sort_by(|a, b| a.name.cmp(&b.name));
            schemas.push(SchemaContents { name, relations });
        }
        Ok(schemas)
    }

    /// The fields of a sample of documents, `_id` first, then in the order
    /// they first appear, each with the BSON type most documents give it.
    async fn columns(&self, relation: &Relation) -> Result<Vec<Column>, DatabaseError> {
        let documents: Vec<Document> = self
            .collection(relation)
            .aggregate([doc! { "$sample": { "size": SAMPLE_SIZE } }])
            .await
            .map_err(|error| self.fail(error))?
            .try_collect()
            .await
            .map_err(|error| self.fail(error))?;
        let mut order: Vec<String> = vec!["_id".into()];
        let mut types: HashMap<String, HashMap<&'static str, usize>> = HashMap::new();
        for document in &documents {
            for (field, value) in document {
                if !order.contains(field) {
                    order.push(field.clone());
                }
                if *value != Bson::Null {
                    *types.entry(field.clone()).or_default().entry(type_name(value)).or_default() += 1;
                }
            }
        }
        Ok(order
            .into_iter()
            .map(|name| {
                let counts = types.remove(&name).unwrap_or_default();
                let mut ranked: Vec<_> = counts.into_iter().collect();
                ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
                let column_type = match ranked.as_slice() {
                    [] => "null".to_string(),
                    [(only, _)] => only.to_string(),
                    [(most, _), ..] => format!("{most} (mixed)"),
                };
                let base = ranked.first().map_or("null", |(name, _)| name);
                Column {
                    is_numeric: matches!(base, "int" | "long" | "double" | "decimal"),
                    kind: match base {
                        "date" => ColumnKind::Timestamp,
                        "bool" => ColumnKind::Boolean,
                        _ => ColumnKind::Other,
                    },
                    enum_values: Vec::new(),
                    column_type,
                    name,
                }
            })
            .collect())
    }

    async fn rows(
        &self,
        relation: &Relation,
        columns: &[Column],
        filters: &[RowFilter],
        sort: Option<&RowSort>,
        offset: u64,
        limit: u64,
    ) -> Result<RowPage, DatabaseError> {
        let filter = filter_document(filters, columns)?;
        let sort = match sort {
            Some(sort) => doc! { sort.column.clone(): if sort.ascending { 1 } else { -1 } },
            // Without an order, pages could overlap as documents move.
            None => doc! { "_id": 1 },
        };
        let documents: Vec<Document> = self
            .collection(relation)
            .find(filter)
            .sort(sort)
            .skip(offset)
            .limit(i64::try_from(limit).unwrap_or(i64::MAX))
            .await
            .map_err(|error| self.fail(error))?
            .try_collect()
            .await
            .map_err(|error| self.fail(error))?;
        let rows = documents
            .iter()
            .map(|document| columns.iter().map(|column| document.get(&column.name).and_then(display).map(|text| cut(&text))).collect())
            .collect();
        Ok(RowPage { rows, offset })
    }

    async fn count(&self, relation: &Relation, columns: &[Column], filters: &[RowFilter]) -> Result<u64, DatabaseError> {
        let filter = filter_document(filters, columns)?;
        self.collection(relation).count_documents(filter).await.map_err(|error| self.fail(error))
    }

    async fn estimated_count(&self, relation: &Relation) -> Result<Option<u64>, DatabaseError> {
        if relation.kind == RelationKind::View {
            return Ok(None);
        }
        self.collection(relation).estimated_document_count().await.map(Some).map_err(|error| self.fail(error))
    }

    /// Updates, deletes and inserts by `_id`. Each update and delete must find
    /// its document. On a replica set they run in one transaction; a single
    /// server has none, so there they stop at the first that fails.
    async fn commit(&self, relation: Relation, changes: PendingChanges, columns: Vec<Column>) -> Result<(), DatabaseError> {
        if self.read_only {
            return Err(DatabaseError::new("This connection is read-only. Nothing was saved."));
        }
        let collection = self.collection(&relation);
        let id_type = columns.iter().find(|column| column.name == "_id").map(|column| column.column_type.clone()).unwrap_or_default();
        let typed = |name: &str, draft: Option<&CellDraft>| -> Result<Bson, DatabaseError> {
            let column_type = columns.iter().find(|column| column.name == name).map_or("", |column| column.column_type.as_str());
            draft_value(draft, column_type)
        };
        let id = |key: &[String]| -> Result<Bson, DatabaseError> {
            let text = key.first().ok_or_else(|| DatabaseError::new("The document has no _id."))?;
            text_value(text, &id_type)
        };
        let not_found = || DatabaseError::new("A document was changed or deleted by someone else. Refresh and try again.");

        let mut session = if self.transactions {
            let mut session = self.client.start_session().await.map_err(|error| self.fail(error))?;
            session.start_transaction().await.map_err(|error| self.fail(error))?;
            Some(session)
        } else {
            None
        };
        let result: Result<(), DatabaseError> = async {
            for update in &changes.updates {
                let mut set = Document::new();
                let mut unset = Document::new();
                for (name, draft) in &update.values {
                    match draft {
                        // A missing field and a null one look the same in the grid; NULL removes it.
                        CellDraft::Null => {
                            unset.insert(name.clone(), "");
                        }
                        _ => {
                            set.insert(name.clone(), typed(name, Some(draft))?);
                        }
                    }
                }
                let mut change = Document::new();
                if !set.is_empty() {
                    change.insert("$set", set);
                }
                if !unset.is_empty() {
                    change.insert("$unset", unset);
                }
                let filter = doc! { "_id": id(&update.key)? };
                let outcome = match session.as_mut() {
                    Some(session) => collection.update_one(filter, change).session(session).await,
                    None => collection.update_one(filter, change).await,
                }
                .map_err(|error| self.fail(error))?;
                if outcome.matched_count == 0 {
                    return Err(not_found());
                }
            }
            for key in &changes.deletes {
                let filter = doc! { "_id": id(key)? };
                let outcome = match session.as_mut() {
                    Some(session) => collection.delete_one(filter).session(session).await,
                    None => collection.delete_one(filter).await,
                }
                .map_err(|error| self.fail(error))?;
                if outcome.deleted_count == 0 {
                    return Err(not_found());
                }
            }
            for row in &changes.inserts {
                let mut document = Document::new();
                for (name, draft) in &row.values {
                    // A default means "leave it out": MongoDB fills _id itself.
                    if !matches!(draft, CellDraft::Default) {
                        document.insert(name.clone(), typed(name, Some(draft))?);
                    }
                }
                match session.as_mut() {
                    Some(session) => collection.insert_one(document).session(session).await,
                    None => collection.insert_one(document).await,
                }
                .map_err(|error| self.fail(error))?;
            }
            Ok(())
        }
        .await;
        if let Some(mut session) = session {
            match &result {
                Ok(()) => session.commit_transaction().await.map_err(|error| self.fail(error))?,
                Err(_) => {
                    if let Err(error) = session.abort_transaction().await {
                        return Err(self.fail(error));
                    }
                }
            }
        }
        result
    }

    /// Whole values for copies: the grid's are cut.
    async fn full_rows(&self, relation: &Relation, columns: &[Column], keys: &[Vec<String>]) -> Result<Vec<Vec<Option<String>>>, DatabaseError> {
        let id_type = columns.iter().find(|column| column.name == "_id").map(|column| column.column_type.clone()).unwrap_or_default();
        let ids = keys.iter().filter_map(|key| key.first()).map(|text| text_value(text, &id_type)).collect::<Result<Vec<_>, _>>()?;
        let documents: Vec<Document> = self
            .collection(relation)
            .find(doc! { "_id": { "$in": ids } })
            .await
            .map_err(|error| self.fail(error))?
            .try_collect()
            .await
            .map_err(|error| self.fail(error))?;
        let by_id: HashMap<String, &Document> =
            documents.iter().filter_map(|document| Some((document.get("_id").and_then(display)?, document))).collect();
        Ok(keys
            .iter()
            .filter_map(|key| by_id.get(key.first()?))
            .map(|document| columns.iter().map(|column| document.get(&column.name).and_then(display)).collect())
            .collect())
    }
}

fn mongo_error(endpoint: &str, error: mongodb::error::Error) -> DatabaseError {
    use mongodb::error::ErrorKind;
    match &*error.kind {
        ErrorKind::Command(command) => DatabaseError::new(command.message.clone()),
        ErrorKind::Write(mongodb::error::WriteFailure::WriteError(write)) => DatabaseError::new(write.message.clone()),
        ErrorKind::Authentication { message, .. } => DatabaseError::new(format!("Authentication failed: {message}")),
        ErrorKind::ServerSelection { .. } => DatabaseError::new(format!("Could not reach {endpoint}: no server answered within the time limit.")),
        ErrorKind::InvalidArgument { message, .. } => DatabaseError::new(message.clone()),
        _ => DatabaseError::new(error.to_string()),
    }
}

/// The BSON type's name, as MongoDB's $type uses it.
fn type_name(value: &Bson) -> &'static str {
    match value {
        Bson::Double(_) => "double",
        Bson::String(_) => "string",
        Bson::Document(_) => "object",
        Bson::Array(_) => "array",
        Bson::Binary(_) => "binData",
        Bson::ObjectId(_) => "objectId",
        Bson::Boolean(_) => "bool",
        Bson::DateTime(_) => "date",
        Bson::Null => "null",
        Bson::RegularExpression(_) => "regex",
        Bson::Int32(_) => "int",
        Bson::Timestamp(_) => "timestamp",
        Bson::Int64(_) => "long",
        Bson::Decimal128(_) => "decimal",
        _ => "other",
    }
}

/// A value as the grid shows it; None for null.
pub(crate) fn display(value: &Bson) -> Option<String> {
    Some(match value {
        Bson::Null | Bson::Undefined => return None,
        Bson::String(text) => text.clone(),
        Bson::Int32(number) => number.to_string(),
        Bson::Int64(number) => number.to_string(),
        Bson::Double(number) => number.to_string(),
        Bson::Decimal128(number) => number.to_string(),
        Bson::Boolean(flag) => flag.to_string(),
        Bson::ObjectId(id) => id.to_hex(),
        Bson::DateTime(date) => date.try_to_rfc3339_string().unwrap_or_else(|_| date.to_string()),
        Bson::Binary(binary) => format!("\\x{}", binary.bytes.iter().map(|byte| format!("{byte:02x}")).collect::<String>()),
        other => other.clone().into_relaxed_extjson().to_string(),
    })
}

fn cut(text: &str) -> String {
    match text.char_indices().nth(GRID_CELL_LIMIT as usize) {
        Some((index, _)) => text[..index].to_string(),
        None => text.to_string(),
    }
}

/// The grid's text as a value of the field's type: "42" stays a number in a
/// number field, a hex string becomes an ObjectId in an ObjectId field, and
/// objects and arrays are read as JSON (mongo shell syntax).
fn text_value(text: &str, column_type: &str) -> Result<Bson, DatabaseError> {
    let base = column_type.split(' ').next().unwrap_or_default();
    let refuse = |what: &str| DatabaseError::new(format!("“{text}” is not {what}."));
    Ok(match base {
        "int" => text.trim().parse::<i32>().map(Bson::Int32).or_else(|_| text.trim().parse::<i64>().map(Bson::Int64)).map_err(|_| refuse("a whole number"))?,
        "long" => Bson::Int64(text.trim().parse().map_err(|_| refuse("a whole number"))?),
        "double" => Bson::Double(text.trim().parse().map_err(|_| refuse("a number"))?),
        "decimal" => Bson::Decimal128(text.trim().parse().map_err(|_| refuse("a decimal number"))?),
        "bool" => match text.trim().to_ascii_lowercase().as_str() {
            "true" => Bson::Boolean(true),
            "false" => Bson::Boolean(false),
            _ => return Err(refuse("true or false")),
        },
        "objectId" => Bson::ObjectId(ObjectId::parse_str(text.trim()).map_err(|_| refuse("an ObjectId (24 hex digits)"))?),
        "date" => Bson::DateTime(mongodb::bson::DateTime::parse_rfc3339_str(text.trim()).map_err(|_| refuse("a date such as 2024-01-02T03:04:05Z"))?),
        "object" | "array" => shell_value(text)?,
        // A string field, or one no sample showed: text, unless it is JSON with braces.
        _ if base == "string" => Bson::String(text.to_string()),
        _ if text.trim_start().starts_with(['{', '[']) => shell_value(text)?,
        _ => Bson::String(text.to_string()),
    })
}

fn draft_value(draft: Option<&CellDraft>, column_type: &str) -> Result<Bson, DatabaseError> {
    Ok(match draft {
        Some(CellDraft::Value { text }) => text_value(text, column_type)?,
        Some(CellDraft::Null) | None => Bson::Null,
        Some(CellDraft::Default) => Bson::Null,
        Some(CellDraft::Function { function }) => match function {
            crate::changes::SqlFunction::Now | crate::changes::SqlFunction::CurrentDate | crate::changes::SqlFunction::CurrentTime => {
                Bson::DateTime(mongodb::bson::DateTime::now())
            }
        },
    })
}

/// The filter bar as a query document, every filter joined with $and.
fn filter_document(filters: &[RowFilter], columns: &[Column]) -> Result<Document, DatabaseError> {
    let mut parts = Vec::new();
    for filter in filters {
        let part = match &filter.target {
            FilterTarget::RawSql => match shell_value(&filter.value)? {
                Bson::Document(document) => document,
                _ => return Err(DatabaseError::new("The raw filter must be a query document, such as { age: { $gt: 30 } }.")),
            },
            FilterTarget::Column { name } => {
                let column_type = columns.iter().find(|column| &column.name == name).map_or("", |column| column.column_type.as_str());
                condition(name, column_type, filter.operator, &filter.value)?
            }
            FilterTarget::AnyColumn => {
                // Text search: a regex over the string fields.
                let any: Vec<Bson> = columns
                    .iter()
                    .filter(|column| column.column_type.starts_with("string"))
                    .map(|column| condition(&column.name, "string", filter.operator, &filter.value).map(Bson::Document))
                    .collect::<Result<_, _>>()?;
                if any.is_empty() {
                    doc! { "_id": { "$exists": false } }
                } else {
                    doc! { "$or": any }
                }
            }
        };
        parts.push(Bson::Document(part));
    }
    Ok(match parts.len() {
        0 => Document::new(),
        1 => match parts.pop() {
            Some(Bson::Document(only)) => only,
            _ => Document::new(),
        },
        _ => doc! { "$and": parts },
    })
}

fn condition(field: &str, column_type: &str, operator: FilterOperator, value: &str) -> Result<Document, DatabaseError> {
    // The typed value, or the text when it doesn't fit the field's type, so
    // a filter on a mixed field still runs.
    let typed = || text_value(value, column_type).unwrap_or_else(|_| Bson::String(value.to_string()));
    let pattern = |prefix: &str, suffix: &str| Bson::RegularExpression(mongodb::bson::Regex {
        pattern: format!("{prefix}{}{suffix}", regex_escape(value)),
        options: "i".into(),
    });
    let list = || -> Vec<Bson> {
        value.split(',').map(str::trim).filter(|item| !item.is_empty()).map(|item| text_value(item, column_type).unwrap_or_else(|_| Bson::String(item.into()))).collect()
    };
    Ok(match operator {
        FilterOperator::Equals => doc! { field: typed() },
        FilterOperator::NotEquals => doc! { field: { "$ne": typed() } },
        FilterOperator::GreaterThan => doc! { field: { "$gt": typed() } },
        FilterOperator::GreaterOrEqual => doc! { field: { "$gte": typed() } },
        FilterOperator::LessThan => doc! { field: { "$lt": typed() } },
        FilterOperator::LessOrEqual => doc! { field: { "$lte": typed() } },
        FilterOperator::Contains => doc! { field: pattern("", "") },
        FilterOperator::NotContains => doc! { field: { "$not": pattern("", "") } },
        FilterOperator::StartsWith => doc! { field: pattern("^", "") },
        FilterOperator::EndsWith => doc! { field: pattern("", "$") },
        FilterOperator::In => doc! { field: { "$in": list() } },
        FilterOperator::NotIn => doc! { field: { "$nin": list() } },
        FilterOperator::Like => {
            let regex: String = value
                .chars()
                .map(|character| match character {
                    '%' => ".*".to_string(),
                    '_' => ".".to_string(),
                    other => regex_escape(&other.to_string()),
                })
                .collect();
            doc! { field: Bson::RegularExpression(mongodb::bson::Regex { pattern: format!("^{regex}$"), options: String::new() }) }
        }
        // null matches a missing field too, as the grid shows both as NULL.
        FilterOperator::IsNull => doc! { field: Bson::Null },
        FilterOperator::IsNotNull => doc! { field: { "$ne": Bson::Null } },
    })
}

fn regex_escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        if "\\^$.|?*+()[]{}/".contains(character) {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

// MARK: - Mongo shell syntax

/// A value written as in the mongo shell: unquoted keys, single quotes,
/// trailing commas, ObjectId("…"), ISODate("…"), NumberLong(…), /regex/.
pub(crate) fn shell_value(text: &str) -> Result<Bson, DatabaseError> {
    let mut parser = ShellParser { chars: text.chars().collect(), index: 0 };
    let value = parser.value()?;
    parser.space();
    if parser.index < parser.chars.len() {
        return Err(parser.error("Unexpected text after the value"));
    }
    Bson::try_from(value).map_err(|error| DatabaseError::new(format!("Not a valid value: {error}")))
}

pub(crate) struct ShellParser {
    pub(crate) chars: Vec<char>,
    pub(crate) index: usize,
}

impl ShellParser {
    pub(crate) fn error(&self, what: &str) -> DatabaseError {
        DatabaseError::new(format!("{what} at character {}.", self.index + 1))
    }

    pub(crate) fn space(&mut self) {
        loop {
            while self.peek().is_some_and(char::is_whitespace) {
                self.index += 1;
            }
            // // and /* */ comments.
            if self.peek() == Some('/') && self.chars.get(self.index + 1) == Some(&'/') {
                while self.peek().is_some_and(|c| c != '\n') {
                    self.index += 1;
                }
            } else if self.peek() == Some('/') && self.chars.get(self.index + 1) == Some(&'*') {
                self.index += 2;
                while self.index < self.chars.len() && !(self.peek() == Some('*') && self.chars.get(self.index + 1) == Some(&'/')) {
                    self.index += 1;
                }
                self.index = (self.index + 2).min(self.chars.len());
            } else {
                break;
            }
        }
    }

    pub(crate) fn peek(&self) -> Option<char> {
        self.chars.get(self.index).copied()
    }

    pub(crate) fn eat(&mut self, character: char) -> bool {
        self.space();
        if self.peek() == Some(character) {
            self.index += 1;
            true
        } else {
            false
        }
    }

    pub(crate) fn expect(&mut self, character: char) -> Result<(), DatabaseError> {
        if self.eat(character) {
            Ok(())
        } else {
            Err(self.error(&format!("Expected “{character}”")))
        }
    }

    pub(crate) fn word(&mut self) -> String {
        self.space();
        let start = self.index;
        while self.peek().is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '$' || c == '.') {
            self.index += 1;
        }
        self.chars[start..self.index].iter().collect()
    }

    fn string(&mut self) -> Result<String, DatabaseError> {
        let quote = self.peek().ok_or_else(|| self.error("Expected a string"))?;
        self.index += 1;
        let mut out = String::new();
        while let Some(character) = self.peek() {
            self.index += 1;
            if character == quote {
                return Ok(out);
            }
            if character == '\\' {
                let next = self.peek().ok_or_else(|| self.error("Unfinished escape"))?;
                self.index += 1;
                match next {
                    'n' => out.push('\n'),
                    't' => out.push('\t'),
                    'r' => out.push('\r'),
                    'u' => {
                        let hex: String = self.chars.get(self.index..self.index + 4).unwrap_or_default().iter().collect();
                        let code = u32::from_str_radix(&hex, 16).map_err(|_| self.error("Bad \\u escape"))?;
                        out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                        self.index += 4;
                    }
                    other => out.push(other),
                }
            } else {
                out.push(character);
            }
        }
        Err(self.error("A string is not closed"))
    }

    pub(crate) fn value(&mut self) -> Result<serde_json::Value, DatabaseError> {
        use serde_json::{json, Value};
        self.space();
        match self.peek() {
            Some('{') => {
                self.index += 1;
                let mut object = serde_json::Map::new();
                loop {
                    if self.eat('}') {
                        break;
                    }
                    self.space();
                    let key = match self.peek() {
                        Some('"' | '\'') => self.string()?,
                        _ => {
                            let word = self.word();
                            if word.is_empty() {
                                return Err(self.error("Expected a field name"));
                            }
                            word
                        }
                    };
                    self.expect(':')?;
                    let value = self.value()?;
                    object.insert(key, value);
                    if !self.eat(',') {
                        self.expect('}')?;
                        break;
                    }
                }
                Ok(Value::Object(object))
            }
            Some('[') => {
                self.index += 1;
                let mut array = Vec::new();
                loop {
                    if self.eat(']') {
                        break;
                    }
                    array.push(self.value()?);
                    if !self.eat(',') {
                        self.expect(']')?;
                        break;
                    }
                }
                Ok(Value::Array(array))
            }
            Some('"' | '\'') => Ok(Value::String(self.string()?)),
            Some('/') => {
                self.index += 1;
                let mut pattern = String::new();
                while let Some(character) = self.peek() {
                    self.index += 1;
                    if character == '/' {
                        break;
                    }
                    if character == '\\' {
                        pattern.push('\\');
                        if let Some(next) = self.peek() {
                            pattern.push(next);
                            self.index += 1;
                        }
                        continue;
                    }
                    pattern.push(character);
                }
                let flags = self.word();
                Ok(json!({ "$regularExpression": { "pattern": pattern, "options": flags } }))
            }
            Some(c) if c == '-' || c.is_ascii_digit() => {
                let start = self.index;
                self.index += 1;
                while self.peek().is_some_and(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-')) {
                    self.index += 1;
                }
                let text: String = self.chars[start..self.index].iter().collect();
                let number: serde_json::Number = text.parse().map_err(|_| self.error("Not a number"))?;
                // A whole number that fits 32 bits is an int, as in the shell's literals.
                if let Some(whole) = number.as_i64() {
                    return Ok(if i32::try_from(whole).is_ok() { json!(whole) } else { json!({ "$numberLong": whole.to_string() }) });
                }
                Ok(Value::Number(number))
            }
            Some(_) => {
                let mut word = self.word();
                if word == "new" {
                    word = self.word();
                }
                match word.as_str() {
                    "true" => Ok(json!(true)),
                    "false" => Ok(json!(false)),
                    "null" | "undefined" => Ok(Value::Null),
                    "ObjectId" | "ISODate" | "Date" | "NumberLong" | "NumberInt" | "NumberDecimal" | "UUID" => {
                        self.expect('(')?;
                        let argument = if self.eat(')') {
                            None
                        } else {
                            let argument = self.value()?;
                            self.expect(')')?;
                            Some(argument)
                        };
                        let text = argument.as_ref().map(|argument| match argument {
                            Value::String(text) => text.clone(),
                            other => other.to_string(),
                        });
                        match (word.as_str(), text) {
                            ("ObjectId", Some(hex)) => Ok(json!({ "$oid": hex })),
                            ("ObjectId", None) => Ok(json!({ "$oid": ObjectId::new().to_hex() })),
                            ("ISODate" | "Date", Some(date)) => Ok(json!({ "$date": date })),
                            ("ISODate" | "Date", None) => Ok(json!({ "$date": { "$numberLong": mongodb::bson::DateTime::now().timestamp_millis().to_string() } })),
                            ("NumberLong", Some(number)) => Ok(json!({ "$numberLong": number })),
                            ("NumberInt", Some(number)) => Ok(json!(number.parse::<i32>().map_err(|_| self.error("Not a whole number"))?)),
                            ("NumberDecimal", Some(number)) => Ok(json!({ "$numberDecimal": number })),
                            ("UUID", Some(uuid)) => Ok(json!({ "$uuid": uuid })),
                            _ => Err(self.error(&format!("{word}() needs a value"))),
                        }
                    }
                    "" => Err(self.error("Expected a value")),
                    other => Err(self.error(&format!("Unknown name “{other}”"))),
                }
            }
            None => Err(self.error("Expected a value")),
        }
    }
}

// MARK: - Structure

impl MongoLink {
    async fn structure(&self, relation: &Relation) -> Result<crate::structure::TableStructure, DatabaseError> {
        use crate::structure::{ColumnDetail, IndexDetail, TableStructure};
        let columns = self
            .columns(relation)
            .await?
            .into_iter()
            .map(|column| ColumnDetail { name: column.name, column_type: column.column_type, nullable: true, default_value: None, comment: None })
            .collect();
        if relation.kind == RelationKind::View {
            return Ok(TableStructure { columns, indexes: Vec::new(), constraints: Vec::new() });
        }
        let indexes: Vec<mongodb::IndexModel> =
            self.collection(relation).list_indexes().await.map_err(|error| self.fail(error))?.try_collect().await.map_err(|error| self.fail(error))?;
        let indexes = indexes
            .into_iter()
            .map(|index| {
                let name = index.options.as_ref().and_then(|options| options.name.clone()).unwrap_or_default();
                let unique = index.options.as_ref().and_then(|options| options.unique).unwrap_or(false);
                let keys = Bson::Document(index.keys.clone()).into_relaxed_extjson().to_string();
                IndexDetail {
                    definition: format!("db.{}.createIndex({keys}{})", relation.name, if unique { ", { unique: true }" } else { "" }),
                    is_primary: name == "_id_",
                    is_unique: unique || name == "_id_",
                    name,
                }
            })
            .collect();
        Ok(TableStructure { columns, indexes, constraints: Vec::new() })
    }

    async fn apply_structure(&self, relation: &Relation, change: crate::structure::StructureChange) -> Result<(), DatabaseError> {
        use crate::structure::StructureChange;
        if self.read_only {
            return Err(DatabaseError::new("This connection is read-only. Nothing was changed."));
        }
        let collection = self.collection(relation);
        match change {
            StructureChange::AddIndex { name, columns, unique } => {
                let mut keys = Document::new();
                for column in columns {
                    keys.insert(column, 1);
                }
                let options = mongodb::options::IndexOptions::builder().name(Some(name).filter(|name| !name.trim().is_empty())).unique(unique).build();
                collection.create_index(mongodb::IndexModel::builder().keys(keys).options(options).build()).await.map_err(|error| self.fail(error))?;
            }
            StructureChange::DropIndex { name, .. } => collection.drop_index(name).await.map_err(|error| self.fail(error))?,
            _ => return Err(DatabaseError::new("MongoDB collections have no fixed columns or constraints; change documents instead.")),
        }
        Ok(())
    }
}

/// The mongo shell commands a structure change runs: indexes only.
pub(crate) fn structure_change_commands(relation: &Relation, change: &crate::structure::StructureChange) -> Result<Vec<String>, DatabaseError> {
    use crate::structure::StructureChange;
    let quote = |text: &str| serde_json::Value::String(text.to_string()).to_string();
    match change {
        StructureChange::AddIndex { name, columns, unique } => {
            if columns.is_empty() {
                return Err(DatabaseError::new("Choose at least one field for the index."));
            }
            let keys = columns.iter().map(|column| format!("{}: 1", quote(column))).collect::<Vec<_>>().join(", ");
            let mut options = Vec::new();
            if !name.trim().is_empty() {
                options.push(format!("name: {}", quote(name.trim())));
            }
            if *unique {
                options.push("unique: true".into());
            }
            let options = if options.is_empty() { String::new() } else { format!(", {{ {} }}", options.join(", ")) };
            Ok(vec![format!("db.getCollection({}).createIndex({{ {keys} }}{options})", quote(&relation.name))])
        }
        StructureChange::DropIndex { name, .. } => Ok(vec![format!("db.getCollection({}).dropIndex({})", quote(&relation.name), quote(name))]),
        _ => Err(DatabaseError::new("MongoDB collections have no fixed columns or constraints; change documents instead.")),
    }
}

// MARK: - Session

impl DatabaseSession {
    pub(crate) async fn mongo_schemas(&self) -> Result<Vec<SchemaContents>, DatabaseError> {
        let link = self.mongo()?;
        on_runtime(async move { link.schemas().await }).await
    }

    pub(crate) async fn mongo_columns(&self, relation: Relation) -> Result<Vec<Column>, DatabaseError> {
        let link = self.mongo()?;
        on_runtime(async move { link.columns(&relation).await }).await
    }

    pub(crate) async fn mongo_rows(
        &self,
        relation: Relation,
        columns: Vec<Column>,
        filters: Vec<RowFilter>,
        sort: Option<RowSort>,
        offset: u64,
        limit: u64,
    ) -> Result<RowPage, DatabaseError> {
        let link = self.mongo()?;
        on_runtime(async move { link.rows(&relation, &columns, &filters, sort.as_ref(), offset, limit).await }).await
    }

    pub(crate) async fn mongo_count(&self, relation: Relation, columns: Vec<Column>, filters: Vec<RowFilter>) -> Result<u64, DatabaseError> {
        let link = self.mongo()?;
        on_runtime(async move { link.count(&relation, &columns, &filters).await }).await
    }

    pub(crate) async fn mongo_estimated_count(&self, relation: Relation) -> Result<Option<u64>, DatabaseError> {
        let link = self.mongo()?;
        on_runtime(async move { link.estimated_count(&relation).await }).await
    }

    pub(crate) async fn mongo_commit(&self, relation: Relation, changes: PendingChanges, columns: Vec<Column>) -> Result<(), DatabaseError> {
        let link = self.mongo()?;
        on_runtime(async move { link.commit(relation, changes, columns).await }).await
    }

    pub(crate) async fn mongo_structure(&self, relation: Relation) -> Result<crate::structure::TableStructure, DatabaseError> {
        let link = self.mongo()?;
        on_runtime(async move { link.structure(&relation).await }).await
    }

    pub(crate) async fn mongo_apply_structure(&self, relation: Relation, change: crate::structure::StructureChange) -> Result<(), DatabaseError> {
        let link = self.mongo()?;
        on_runtime(async move { link.apply_structure(&relation, change).await }).await
    }

    pub(crate) async fn mongo_full_rows(
        &self,
        relation: Relation,
        columns: Vec<Column>,
        keys: Vec<Vec<String>>,
    ) -> Result<Vec<Vec<Option<String>>>, DatabaseError> {
        let link = self.mongo()?;
        on_runtime(async move { link.full_rows(&relation, &columns, &keys).await }).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_shell_syntax() {
        let value = shell_value("{ age: { $gt: 30 }, 'name': /^ad/i, _id: ObjectId('65a1b2c3d4e5f60718293a4b'), big: 5000000000, tags: ['a', \"b\",], }").unwrap();
        let Bson::Document(document) = value else { panic!("a document") };
        assert_eq!(document.get_document("age").unwrap().get_i32("$gt").unwrap(), 30);
        assert!(matches!(document.get("name"), Some(Bson::RegularExpression(regex)) if regex.pattern == "^ad" && regex.options == "i"));
        assert!(matches!(document.get("_id"), Some(Bson::ObjectId(_))));
        assert_eq!(document.get_i64("big").unwrap(), 5_000_000_000);
        assert_eq!(document.get_array("tags").unwrap().len(), 2);
        assert!(matches!(shell_value("ISODate('2024-01-02T03:04:05Z')").unwrap(), Bson::DateTime(_)));
        assert!(shell_value("{ a: }").is_err());
    }

    #[test]
    fn keeps_a_fields_type() {
        assert_eq!(text_value("42", "int").unwrap(), Bson::Int32(42));
        assert_eq!(text_value("42", "string").unwrap(), Bson::String("42".into()));
        assert_eq!(text_value("1.5", "double (mixed)").unwrap(), Bson::Double(1.5));
        assert!(text_value("x", "int").is_err());
        assert!(matches!(text_value("65a1b2c3d4e5f60718293a4b", "objectId").unwrap(), Bson::ObjectId(_)));
        assert!(matches!(text_value("{a: 1}", "object").unwrap(), Bson::Document(_)));
        assert_eq!(regex_escape("a.b*"), "a\\.b\\*");
    }
}
