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

    pub(crate) fn string(&mut self) -> Result<String, DatabaseError> {
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

// MARK: - Commands for review

/// `use database` and a call on the collection, as the editor runs them.
pub(crate) fn shell_command(relation: &Relation, call: &str) -> String {
    let quote = |text: &str| serde_json::Value::String(text.to_string()).to_string();
    format!("use {}\ndb.getCollection({}).{call}", quote(&relation.schema), quote(&relation.name))
}

fn typed_json(name: &str, draft: Option<&CellDraft>, columns: &[Column]) -> Result<String, DatabaseError> {
    let column_type = columns.iter().find(|column| column.name == name).map_or("", |column| column.column_type.as_str());
    Ok(draft_value(draft, column_type)?.into_relaxed_extjson().to_string())
}

/// The commands a commit runs, for Review Changes.
pub(crate) fn change_commands(changes: &PendingChanges, relation: &Relation, columns: &[Column]) -> Result<Vec<String>, DatabaseError> {
    let quote = |text: &str| serde_json::Value::String(text.to_string()).to_string();
    let collection = format!("db.getCollection({})", quote(&relation.name));
    let id = |key: &[String]| -> Result<String, DatabaseError> {
        let text = key.first().ok_or_else(|| DatabaseError::new("The document has no _id."))?;
        typed_json("_id", Some(&CellDraft::Value { text: text.clone() }), columns)
    };
    let mut commands = Vec::new();
    for update in &changes.updates {
        let mut set = Vec::new();
        let mut unset = Vec::new();
        let mut names: Vec<&String> = update.values.keys().collect();
        names.sort();
        for name in names {
            match &update.values[name] {
                CellDraft::Null => unset.push(format!("{}: \"\"", quote(name))),
                draft => set.push(format!("{}: {}", quote(name), typed_json(name, Some(draft), columns)?)),
            }
        }
        let mut parts = Vec::new();
        if !set.is_empty() {
            parts.push(format!("$set: {{ {} }}", set.join(", ")));
        }
        if !unset.is_empty() {
            parts.push(format!("$unset: {{ {} }}", unset.join(", ")));
        }
        commands.push(format!("{collection}.updateOne({{ _id: {} }}, {{ {} }})", id(&update.key)?, parts.join(", ")));
    }
    for key in &changes.deletes {
        commands.push(format!("{collection}.deleteOne({{ _id: {} }})", id(key)?));
    }
    for row in &changes.inserts {
        commands.push(insert_command(row, relation, columns)?);
    }
    Ok(commands)
}

pub(crate) fn insert_command(row: &crate::changes::NewRow, relation: &Relation, columns: &[Column]) -> Result<String, DatabaseError> {
    let quote = |text: &str| serde_json::Value::String(text.to_string()).to_string();
    let mut fields = Vec::new();
    let mut names: Vec<&String> = row.values.keys().collect();
    names.sort();
    for name in names {
        let draft = &row.values[name];
        if !matches!(draft, CellDraft::Default) {
            fields.push(format!("{}: {}", quote(name), typed_json(name, Some(draft), columns)?));
        }
    }
    Ok(format!("db.getCollection({}).insertOne({{ {} }})", quote(&relation.name), fields.join(", ")))
}

// MARK: - Query editor

/// One call in mongo shell syntax: `db.people.find({ age: { $gt: 30 } }).sort({ age: -1 }).limit(5)`.
struct ShellCall {
    /// None for database-level calls (db.runCommand, show dbs, use).
    collection: Option<String>,
    method: String,
    arguments: Vec<Bson>,
    /// .sort(), .limit(), .skip(), .projection() after find.
    modifiers: Vec<(String, Vec<Bson>)>,
}

/// Methods that change data; refused on read-only connections and to AI tools.
const WRITE_METHODS: &[&str] = &[
    "insertOne", "insertMany", "updateOne", "updateMany", "replaceOne", "deleteOne", "deleteMany", "findOneAndUpdate",
    "findOneAndReplace", "findOneAndDelete", "bulkWrite", "createIndex", "createIndexes", "dropIndex", "dropIndexes", "drop",
    "renameCollection", "runCommand", "dropDatabase", "createCollection",
];

/// Read methods, the only ones AI tools may call.
const READ_METHODS: &[&str] =
    &["find", "findOne", "aggregate", "countDocuments", "estimatedDocumentCount", "distinct", "getCollectionNames", "showDatabases", "showCollections"];

/// The calls in an editor's text, one per line or separated by `;`.
fn parse_script(text: &str) -> Result<Vec<ShellCall>, DatabaseError> {
    let mut parser = ShellParser { chars: text.chars().collect(), index: 0 };
    let mut calls = Vec::new();
    loop {
        parser.space();
        while parser.eat(';') {
            parser.space();
        }
        if parser.index >= parser.chars.len() {
            break;
        }
        calls.push(parse_call(&mut parser)?);
    }
    if calls.is_empty() {
        return Err(DatabaseError::new("There is nothing to run. Try db.collection.find()."));
    }
    Ok(calls)
}

fn parse_call(parser: &mut ShellParser) -> Result<ShellCall, DatabaseError> {
    let arguments = |parser: &mut ShellParser| -> Result<Vec<Bson>, DatabaseError> {
        parser.expect('(')?;
        let mut values = Vec::new();
        loop {
            if parser.eat(')') {
                break;
            }
            let value = parser.value()?;
            values.push(Bson::try_from(value).map_err(|error| DatabaseError::new(format!("Not a valid value: {error}")))?);
            if !parser.eat(',') {
                parser.expect(')')?;
                break;
            }
        }
        Ok(values)
    };
    let first = parser.word();
    // show dbs, show collections, use name
    if first == "show" {
        let what = parser.word();
        let method = match what.as_str() {
            "dbs" | "databases" => "showDatabases",
            "collections" | "tables" => "showCollections",
            _ => return Err(parser.error("show takes dbs or collections")),
        };
        return Ok(ShellCall { collection: None, method: method.into(), arguments: Vec::new(), modifiers: Vec::new() });
    }
    if first == "use" {
        // use shop, or use "my-shop" for names a word can't hold.
        parser.space();
        let name = if matches!(parser.peek(), Some('"' | '\'')) {
            parser.value()?.as_str().unwrap_or_default().to_string()
        } else {
            parser.word()
        };
        return Ok(ShellCall { collection: None, method: "use".into(), arguments: vec![Bson::String(name)], modifiers: Vec::new() });
    }
    // The parser reads dots into words: "db.people.find" is one word.
    let mut parts: Vec<String> = first.split('.').map(str::to_string).collect();
    if parts.first().map(String::as_str) != Some("db") {
        return Err(parser.error("A command starts with db., show or use"));
    }
    parts.remove(0);
    let mut collection = None;
    // db.getCollection("name").find(…) and db["name"].find(…)
    if parts.first().map(String::as_str) == Some("getCollection") && parts.len() == 1 {
        let name = arguments(parser)?;
        collection = name.first().and_then(|name| name.as_str().map(str::to_string));
        if !parser.eat('.') {
            return Err(parser.error("Expected .method() after getCollection()"));
        }
        parts = parser.word().split('.').map(str::to_string).collect();
    } else if parts.is_empty() && parser.eat('[') {
        let value = parser.value()?;
        parser.expect(']')?;
        collection = value.as_str().map(str::to_string);
        if !parser.eat('.') {
            return Err(parser.error("Expected .method() after db[…]"));
        }
        parts = parser.word().split('.').map(str::to_string).collect();
    }
    let method = parts.pop().ok_or_else(|| parser.error("Expected a method"))?;
    if collection.is_none() && !parts.is_empty() {
        collection = Some(parts.join("."));
    }
    let arguments_list = arguments(parser)?;
    let mut modifiers = Vec::new();
    while parser.eat('.') {
        let name = parser.word();
        modifiers.push((name, arguments(parser)?));
    }
    Ok(ShellCall { collection, method, arguments: arguments_list, modifiers })
}

fn document_argument(call: &ShellCall, index: usize) -> Result<Document, DatabaseError> {
    match call.arguments.get(index) {
        None | Some(Bson::Null) => Ok(Document::new()),
        Some(Bson::Document(document)) => Ok(document.clone()),
        Some(_) => Err(DatabaseError::new(format!("Argument {} of {}() must be a document.", index + 1, call.method))),
    }
}

/// Documents as a result set: the fields in the order they first appear.
fn result_set(documents: &[Document], total: u64) -> crate::query::ResultSet {
    let mut columns: Vec<String> = Vec::new();
    for document in documents {
        for field in document.keys() {
            if !columns.contains(field) {
                columns.push(field.clone());
            }
        }
    }
    let numeric = columns
        .iter()
        .map(|column| {
            documents.iter().filter_map(|document| document.get(column)).find(|value| **value != Bson::Null).is_some_and(|value| {
                matches!(value, Bson::Int32(_) | Bson::Int64(_) | Bson::Double(_) | Bson::Decimal128(_))
            })
        })
        .collect();
    let rows = documents.iter().map(|document| columns.iter().map(|column| document.get(column).and_then(display).map(|text| cut(&text))).collect()).collect();
    crate::query::ResultSet { columns, rows, total_rows: total, numeric_columns: numeric }
}

impl MongoLink {
    /// The query editor's Run: each call in turn. A single server has no
    /// transactions, so a failing call doesn't undo the ones before it.
    async fn run_script(&self, default_database: String, text: String, max_rows: u64, read_only: bool) -> Result<crate::query::QueryResult, DatabaseError> {
        let started = std::time::Instant::now();
        let calls = parse_script(&text)?;
        let mut database = default_database;
        let mut result_sets = Vec::new();
        let mut statement_counts = Vec::new();
        let mut verbs = Vec::new();
        for call in &calls {
            if (read_only || self.read_only) && !READ_METHODS.contains(&call.method.as_str()) && call.method != "use" {
                return Err(DatabaseError::new(format!("{}() isn't allowed here: only reads are.", call.method)));
            }
            verbs.push(call.method.clone());
            if call.method == "use" {
                database = call.arguments.first().and_then(|name| name.as_str()).unwrap_or_default().to_string();
                statement_counts.push(0);
                continue;
            }
            let (set, count) = self.run_call(&database, call, max_rows, read_only).await?;
            statement_counts.push(count);
            if let Some(set) = set {
                result_sets.push(set);
            }
        }
        Ok(crate::query::QueryResult {
            result_sets,
            statement_counts,
            statement_verbs: verbs,
            elapsed_ms: started.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
        })
    }

    async fn run_call(&self, database: &str, call: &ShellCall, max_rows: u64, read_only: bool) -> Result<(Option<crate::query::ResultSet>, u64), DatabaseError> {
        let fail = |error| self.fail(error);
        let db = self.client.database(if database.is_empty() { "test" } else { database });
        let collection = || -> Result<mongodb::Collection<Document>, DatabaseError> {
            let name = call.collection.as_deref().ok_or_else(|| DatabaseError::new(format!("{}() needs a collection: db.name.{}()", call.method, call.method)))?;
            Ok(db.collection(name))
        };
        let one_value = |name: &str, value: Bson| -> (Option<crate::query::ResultSet>, u64) {
            (Some(result_set(&[doc! { name: value }], 1)), 1)
        };
        // AI tools get a time limit; the editor has Cancel… which MongoDB lacks, so a long one too.
        let time_limit = Duration::from_secs(if read_only { 30 } else { 300 });
        let limit = i64::try_from(max_rows).unwrap_or(i64::MAX);
        Ok(match call.method.as_str() {
            "find" | "findOne" => {
                let filter = document_argument(call, 0)?;
                let mut find = mongodb::options::FindOptions::default();
                if let Some(Bson::Document(projection)) = call.arguments.get(1) {
                    find.projection = Some(projection.clone());
                }
                let mut user_limit = if call.method == "findOne" { Some(1) } else { None };
                for (name, arguments) in &call.modifiers {
                    let first = arguments.first();
                    match (name.as_str(), first) {
                        ("sort", Some(Bson::Document(sort))) => find.sort = Some(sort.clone()),
                        ("projection" | "project", Some(Bson::Document(projection))) => find.projection = Some(projection.clone()),
                        ("limit", Some(value)) => user_limit = value.as_i64().or_else(|| value.as_i32().map(i64::from)).or_else(|| value.as_f64().map(|value| value as i64)),
                        ("skip", Some(value)) => find.skip = value.as_i64().or_else(|| value.as_i32().map(i64::from)).map(|skip| skip as u64),
                        ("toArray" | "pretty", _) => {}
                        (other, _) => return Err(DatabaseError::new(format!(".{other}() isn't supported after find()."))),
                    }
                }
                find.limit = Some(user_limit.map_or(limit, |user| user.min(limit)));
                find.max_time = Some(time_limit);
                let documents: Vec<Document> =
                    collection()?.find(filter.clone()).with_options(find.clone()).await.map_err(fail)?.try_collect().await.map_err(fail)?;
                // The real total, as the grid's footer shows it, when the page is full.
                let total = if user_limit.is_none() && documents.len() as u64 >= max_rows {
                    let skip = find.skip.unwrap_or(0);
                    collection()?.count_documents(filter).await.map_err(fail)?.saturating_sub(skip)
                } else {
                    documents.len() as u64
                };
                (Some(result_set(&documents, total)), total)
            }
            "aggregate" => {
                let pipeline: Vec<Document> = match call.arguments.first() {
                    Some(Bson::Array(stages)) => stages.iter().filter_map(|stage| stage.as_document().cloned()).collect(),
                    // Stages as separate arguments, as the old shell took them.
                    _ => call.arguments.iter().filter_map(|stage| stage.as_document().cloned()).collect(),
                };
                if read_only || self.read_only {
                    if let Some(stage) = pipeline.iter().flat_map(|stage| stage.keys()).find(|key| *key == "$out" || *key == "$merge") {
                        return Err(DatabaseError::new(format!("{stage} writes to a collection; only reads are allowed here.")));
                    }
                }
                let mut cursor = collection()?.aggregate(pipeline).max_time(time_limit).await.map_err(fail)?;
                let mut documents = Vec::new();
                let mut total = 0u64;
                while let Some(document) = cursor.try_next().await.map_err(fail)? {
                    if total < max_rows {
                        documents.push(document);
                    }
                    total += 1;
                }
                (Some(result_set(&documents, total)), total)
            }
            "countDocuments" => {
                let count = collection()?.count_documents(document_argument(call, 0)?).max_time(time_limit).await.map_err(fail)?;
                one_value("count", Bson::Int64(count as i64))
            }
            "estimatedDocumentCount" => {
                let count = collection()?.estimated_document_count().await.map_err(fail)?;
                one_value("count", Bson::Int64(count as i64))
            }
            "distinct" => {
                let field = call.arguments.first().and_then(|field| field.as_str()).ok_or_else(|| DatabaseError::new("distinct() needs a field name."))?;
                let values = collection()?.distinct(field, document_argument(call, 1)?).max_time(time_limit).await.map_err(fail)?;
                let documents: Vec<Document> = values.into_iter().take(usize::try_from(max_rows).unwrap_or(usize::MAX)).map(|value| doc! { field: value }).collect();
                let count = documents.len() as u64;
                (Some(result_set(&documents, count)), count)
            }
            "getCollectionNames" | "showCollections" => {
                let mut names = db.list_collection_names().await.map_err(fail)?;
                names.sort();
                let documents: Vec<Document> = names.into_iter().map(|name| doc! { "collection": name }).collect();
                let count = documents.len() as u64;
                (Some(result_set(&documents, count)), count)
            }
            "showDatabases" => {
                let mut names = self.client.list_database_names().await.map_err(fail)?;
                names.sort();
                let documents: Vec<Document> = names.into_iter().map(|name| doc! { "database": name }).collect();
                let count = documents.len() as u64;
                (Some(result_set(&documents, count)), count)
            }
            "insertOne" => {
                let result = collection()?.insert_one(document_argument(call, 0)?).await.map_err(fail)?;
                (Some(result_set(&[doc! { "insertedId": result.inserted_id }], 1)), 1)
            }
            "insertMany" => {
                let documents: Vec<Document> = match call.arguments.first() {
                    Some(Bson::Array(documents)) => documents.iter().filter_map(|document| document.as_document().cloned()).collect(),
                    _ => return Err(DatabaseError::new("insertMany() takes an array of documents.")),
                };
                let result = collection()?.insert_many(documents).await.map_err(fail)?;
                (None, result.inserted_ids.len() as u64)
            }
            "updateOne" | "updateMany" => {
                let (filter, update) = (document_argument(call, 0)?, document_argument(call, 1)?);
                let result = if call.method == "updateOne" {
                    collection()?.update_one(filter, update).await
                } else {
                    collection()?.update_many(filter, update).await
                }
                .map_err(fail)?;
                (Some(result_set(&[doc! { "matched": result.matched_count as i64, "modified": result.modified_count as i64 }], 1)), result.modified_count)
            }
            "replaceOne" => {
                let result = collection()?.replace_one(document_argument(call, 0)?, document_argument(call, 1)?).await.map_err(fail)?;
                (Some(result_set(&[doc! { "matched": result.matched_count as i64, "modified": result.modified_count as i64 }], 1)), result.modified_count)
            }
            "deleteOne" | "deleteMany" => {
                let filter = document_argument(call, 0)?;
                let result = if call.method == "deleteOne" { collection()?.delete_one(filter).await } else { collection()?.delete_many(filter).await }.map_err(fail)?;
                (None, result.deleted_count)
            }
            "createIndex" => {
                let keys = document_argument(call, 0)?;
                let mut model = mongodb::IndexModel::builder().keys(keys).build();
                if let Some(Bson::Document(options)) = call.arguments.get(1) {
                    model.options = Some(mongodb::bson::from_document(options.clone()).map_err(|error| DatabaseError::new(format!("Index options: {error}")))?);
                }
                let result = collection()?.create_index(model).await.map_err(fail)?;
                one_value("index", Bson::String(result.index_name))
            }
            "dropIndex" => {
                let name = call.arguments.first().and_then(|name| name.as_str()).ok_or_else(|| DatabaseError::new("dropIndex() takes the index's name."))?;
                collection()?.drop_index(name).await.map_err(fail)?;
                (None, 0)
            }
            "drop" => {
                collection()?.drop().await.map_err(fail)?;
                (None, 0)
            }
            "runCommand" => {
                let reply = db.run_command(document_argument(call, 0)?).await.map_err(fail)?;
                (Some(result_set(&[reply], 1)), 1)
            }
            other => return Err(DatabaseError::new(format!("{other}() isn't supported in TablePort's editor."))),
        })
    }

    /// AI access: one read call, its documents as relaxed extended JSON.
    async fn read_json(&self, default_database: String, text: String, max_rows: u64) -> Result<Vec<serde_json::Value>, DatabaseError> {
        let calls = parse_script(&text)?;
        let [call] = calls.as_slice() else {
            return Err(DatabaseError::new("Give one call, such as db.people.find({ age: { $gt: 30 } })."));
        };
        if !READ_METHODS.contains(&call.method.as_str()) {
            return Err(DatabaseError::new(format!("{}() isn't allowed: AI access only reads.", call.method)));
        }
        if WRITE_METHODS.contains(&call.method.as_str()) {
            return Err(DatabaseError::new(format!("{}() writes; AI access only reads.", call.method)));
        }
        let (set, _) = self.run_call(&default_database, call, max_rows, true).await?;
        let Some(set) = set else { return Ok(Vec::new()) };
        Ok(set
            .rows
            .into_iter()
            .map(|row| serde_json::Value::Object(set.columns.iter().cloned().zip(row.into_iter().map(|cell| cell.map_or(serde_json::Value::Null, serde_json::Value::String))).collect()))
            .collect())
    }

    /// Writes a collection's documents to `path`, as they arrive.
    async fn export(
        &self,
        relation: &Relation,
        columns: &[Column],
        filters: &[RowFilter],
        sort: Option<&RowSort>,
        scope: &crate::export::ExportScope,
        format: crate::export::ExportFormat,
        path: &str,
    ) -> Result<u64, DatabaseError> {
        use crate::export::{ExportFormat, ExportScope};
        use tokio::io::AsyncWriteExt;
        if format == ExportFormat::Sql {
            return Err(DatabaseError::new("MongoDB documents export as JSON or CSV."));
        }
        let id_type = columns.iter().find(|column| column.name == "_id").map(|column| column.column_type.clone()).unwrap_or_default();
        let mut find = mongodb::options::FindOptions::default();
        find.sort = Some(match sort {
            Some(sort) => doc! { sort.column.clone(): if sort.ascending { 1 } else { -1 } },
            None => doc! { "_id": 1 },
        });
        let filter = match scope {
            ExportScope::Keys { keys } => {
                let ids = keys.iter().filter_map(|key| key.first()).map(|text| text_value(text, &id_type)).collect::<Result<Vec<_>, _>>()?;
                doc! { "_id": { "$in": ids } }
            }
            ExportScope::Page { offset, limit } => {
                find.skip = Some(*offset);
                find.limit = Some(i64::try_from(*limit).unwrap_or(i64::MAX));
                filter_document(filters, columns)?
            }
            ExportScope::All => filter_document(filters, columns)?,
        };
        let write_error = |error: std::io::Error| DatabaseError::new(format!("Can't write {path}: {error}."));
        let file = tokio::fs::File::create(path).await.map_err(|error| DatabaseError::new(format!("Can't create {path}: {error}.")))?;
        let mut out = tokio::io::BufWriter::new(file);
        let mut cursor = self.collection(relation).find(filter).with_options(find).await.map_err(|error| self.fail(error))?;
        let mut count = 0u64;
        if format == ExportFormat::Csv {
            let header = columns.iter().map(|column| crate::sqlite::csv_field(&column.name)).collect::<Vec<_>>().join(",");
            out.write_all(format!("{header}\n").as_bytes()).await.map_err(write_error)?;
        } else {
            out.write_all(b"[").await.map_err(write_error)?;
        }
        while let Some(document) = cursor.try_next().await.map_err(|error| self.fail(error))? {
            let line = match format {
                ExportFormat::Csv => {
                    let fields = columns
                        .iter()
                        .map(|column| document.get(&column.name).and_then(display).as_deref().map(crate::sqlite::csv_field).unwrap_or_default())
                        .collect::<Vec<_>>();
                    format!("{}\n", fields.join(","))
                }
                // Whole documents, in relaxed extended JSON: dates and ObjectIds keep their type.
                _ => format!("{}{}", if count == 0 { "\n  " } else { ",\n  " }, Bson::Document(document).into_relaxed_extjson()),
            };
            out.write_all(line.as_bytes()).await.map_err(write_error)?;
            count += 1;
        }
        if format == ExportFormat::Json {
            out.write_all(if count == 0 { b"]\n" } else { b"\n]\n" }).await.map_err(write_error)?;
        }
        out.flush().await.map_err(write_error)?;
        Ok(count)
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

    pub(crate) async fn mongo_run(&self, text: String, max_rows: u64) -> Result<crate::query::QueryResult, DatabaseError> {
        let link = self.mongo()?;
        let database = self.settings().database;
        on_runtime(async move { link.run_script(database, text, max_rows, false).await }).await
    }

    /// AI access: one read call.
    pub(crate) async fn mongo_read(&self, text: String, max_rows: u64) -> Result<Vec<serde_json::Value>, DatabaseError> {
        let link = self.mongo()?;
        let database = self.settings().database;
        on_runtime(async move { link.read_json(database, text, max_rows).await }).await
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn mongo_export(
        &self,
        relation: Relation,
        columns: Vec<Column>,
        filters: Vec<RowFilter>,
        sort: Option<RowSort>,
        scope: crate::export::ExportScope,
        format: crate::export::ExportFormat,
        path: String,
    ) -> Result<u64, DatabaseError> {
        let link = self.mongo()?;
        on_runtime(async move { link.export(&relation, &columns, &filters, sort.as_ref(), &scope, format, &path).await }).await
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
