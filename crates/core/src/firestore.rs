//! Google Cloud Firestore, through its REST API. A connection is to one
//! database of one project; its root collections show as tables, and their
//! fields, sampled from the documents, as columns, as for MongoDB. The
//! document ID is the column `__name__`, Firestore's own name for it.
//!
//! Google signs in with a service account: TablePort signs a JWT with the
//! account's key and trades it for an access token, kept until it expires.
//! The emulator needs no sign-in.
//!
//! Firestore has no OFFSET-free scan, no substring match and no query
//! language. The grid pages with offset, the comparisons Firestore can do go
//! to it, and the rest run here on documents read with a cursor. The query
//! editor takes the Firebase SDK's own chain: db.collection('users')
//! .where('age', '>', 30).orderBy('name').limit(10).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use serde_json::{json, Map, Value};

use crate::changes::{CellDraft, NewRow, PendingChanges, SqlFunction};
use crate::mongo::ShellParser;
use crate::rows::{Column, ColumnKind, FilterOperator, FilterTarget, RowFilter, RowPage, RowSort};
use crate::schema::{Relation, RelationKind, SchemaContents};
use crate::session::ConnectionSettings;
use crate::structure::{ColumnDetail, TableStructure};
use crate::{DatabaseError, DatabaseSession};

/// The document ID's column: Firestore's name for the key in queries.
pub(crate) const ID: &str = "__name__";
/// Documents read to learn a collection's fields.
const SAMPLE: u64 = 100;
const SCAN_PAGE: u64 = 300;
const ROW_CHANGED: &str = "A document was changed or deleted by someone else. Nothing was saved; refresh and try again.";

#[derive(Clone)]
pub(crate) struct FirestoreLink {
    client: reqwest::Client,
    /// …/v1/projects/{project}/databases/{database}/documents
    base: String,
    /// projects/{project}/databases/{database}/documents: the prefix of every document name.
    root: String,
    account: Option<Arc<ServiceAccount>>,
    token: Arc<Mutex<Option<(String, Instant)>>>,
    pub(crate) version: String,
    pub(crate) database: String,
    read_only: bool,
    endpoint: String,
    closed: Arc<AtomicBool>,
}

struct ServiceAccount {
    email: String,
    key: ring::signature::RsaKeyPair,
    token_uri: String,
}

impl FirestoreLink {
    /// `user` is the project ID, `database` the database ID ("(default)"
    /// when empty), `password` the service account's JSON key, and `host`
    /// the emulator's host:port when it is set.
    pub(crate) async fn open(settings: &ConnectionSettings) -> Result<Self, DatabaseError> {
        let key: Option<Value> = match settings.password.as_deref().map(str::trim).filter(|key| !key.is_empty()) {
            Some(text) => Some(serde_json::from_str(text).map_err(|_| DatabaseError::new("The service account key isn't valid JSON."))?),
            None => None,
        };
        let project = Some(settings.user.trim().to_string())
            .filter(|project| !project.is_empty())
            .or_else(|| key.as_ref().and_then(|key| key["project_id"].as_str()).map(str::to_string))
            .ok_or_else(|| DatabaseError::new("Firestore needs the project ID."))?;
        let database = Some(settings.database.trim()).filter(|name| !name.is_empty()).unwrap_or("(default)").to_string();
        let emulator = settings.host.trim();
        let (origin, account) = if emulator.is_empty() {
            let key = key.ok_or_else(|| DatabaseError::new("Firestore needs a service account key, unless it is the emulator."))?;
            (String::from("https://firestore.googleapis.com"), Some(Arc::new(service_account(&key)?)))
        } else {
            (format!("http://{emulator}"), None)
        };
        let root = format!("projects/{project}/databases/{database}/documents");
        let link = Self {
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(120))
                .build()
                .map_err(|error| DatabaseError::new(format!("Could not set up HTTPS: {error}")))?,
            base: format!("{origin}/v1/{}", root.replace("(default)", "%28default%29")),
            root,
            account,
            token: Arc::new(Mutex::new(None)),
            version: if emulator.is_empty() { "Firestore".into() } else { "Firestore emulator".into() },
            database,
            read_only: settings.read_only,
            endpoint: settings.endpoint(),
            closed: Arc::new(AtomicBool::new(false)),
        };
        // A first call checks the key, the project and the database.
        link.collection_ids("").await?;
        Ok(link)
    }

    /// The access token, renewed a minute before it expires. The emulator
    /// takes "owner", which grants every right.
    async fn bearer(&self) -> Result<String, DatabaseError> {
        let Some(account) = &self.account else { return Ok("owner".into()) };
        if let Some((token, expires)) = self.token.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).as_ref() {
            if Instant::now() + Duration::from_secs(60) < *expires {
                return Ok(token.clone());
            }
        }
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|elapsed| elapsed.as_secs()).unwrap_or(0);
        let encode = |value: &Value| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value.to_string());
        let unsigned = format!(
            "{}.{}",
            encode(&json!({ "alg": "RS256", "typ": "JWT" })),
            encode(&json!({
                "iss": account.email,
                "scope": "https://www.googleapis.com/auth/datastore",
                "aud": account.token_uri,
                "iat": now,
                "exp": now + 3600,
            }))
        );
        let mut signature = vec![0; account.key.public().modulus_len()];
        account
            .key
            .sign(&ring::signature::RSA_PKCS1_SHA256, &ring::rand::SystemRandom::new(), unsigned.as_bytes(), &mut signature)
            .map_err(|_| DatabaseError::new("The service account key couldn't sign the sign-in request."))?;
        let assertion = format!("{unsigned}.{}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(signature));
        let response = self
            .client
            .post(&account.token_uri)
            .header("content-type", "application/x-www-form-urlencoded")
            // The assertion is base64url and dots, which need no escaping.
            .body(format!("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer&assertion={assertion}"))
            .send()
            .await
            .map_err(|error| DatabaseError::new(format!("Could not reach Google's sign-in: {error}")))?;
        let body = response.text().await.map_err(|error| DatabaseError::new(format!("Unreadable sign-in reply: {error}")))?;
        let reply: Value = serde_json::from_str(&body).map_err(|error| DatabaseError::new(format!("Unreadable sign-in reply: {error}")))?;
        let token = reply["access_token"].as_str().ok_or_else(|| {
            DatabaseError::new(format!("Google refused the service account: {}", reply["error_description"].as_str().unwrap_or("no reason given")))
        })?;
        let lifetime = reply["expires_in"].as_u64().unwrap_or(3600);
        *self.token.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some((token.to_string(), Instant::now() + Duration::from_secs(lifetime)));
        Ok(token.to_string())
    }

    /// POSTs `body` to the documents URL plus `suffix` (":runQuery", "/users/ada:…").
    async fn post(&self, suffix: &str, body: Value) -> Result<Value, DatabaseError> {
        if self.is_closed() {
            return Err(DatabaseError::new(format!("The connection to {} is closed.", self.endpoint)));
        }
        let bearer = self.bearer().await?;
        let response = self
            .client
            .post(format!("{}{suffix}", self.base))
            .bearer_auth(bearer)
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .map_err(|error| DatabaseError::new(format!("Could not reach {}: {error}", self.endpoint)))?;
        let status = response.status();
        let text = response.text().await.map_err(|error| DatabaseError::new(format!("Could not read the reply: {error}")))?;
        let reply: Value = serde_json::from_str(&text).unwrap_or(Value::String(text.clone()));
        if !status.is_success() {
            let error = if reply.is_array() { &reply[0]["error"] } else { &reply["error"] };
            let message = error["message"].as_str().map(str::to_string).unwrap_or_else(|| format!("{status}: {}", text.trim()));
            let code = error["status"].as_str().unwrap_or_default();
            return Err(DatabaseError::Failed { message, detail: Some(code.to_string()).filter(|code| !code.is_empty()), hint: None, position: None });
        }
        Ok(reply)
    }

    /// The collections under `parent` ("" for the root, or a document's path).
    async fn collection_ids(&self, parent: &str) -> Result<Vec<String>, DatabaseError> {
        let suffix = if parent.is_empty() { ":listCollectionIds".to_string() } else { format!("/{}:listCollectionIds", encode_path(parent)) };
        let mut ids = Vec::new();
        let mut page_token: Option<String> = None;
        loop {
            let mut body = json!({ "pageSize": 300 });
            if let Some(token) = &page_token {
                body["pageToken"] = json!(token);
            }
            let reply = self.post(&suffix, body).await?;
            ids.extend(reply["collectionIds"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_string));
            match reply["nextPageToken"].as_str().filter(|token| !token.is_empty()) {
                Some(token) => page_token = Some(token.to_string()),
                None => break,
            }
        }
        ids.sort();
        Ok(ids)
    }

    /// The documents of a structured query, with their names.
    async fn run_query(&self, collection: &str, query: Value) -> Result<Vec<Value>, DatabaseError> {
        let (parent, id) = split_collection(collection);
        let mut query = query;
        query["from"] = json!([{ "collectionId": id }]);
        let suffix = if parent.is_empty() { ":runQuery".to_string() } else { format!("/{}:runQuery", encode_path(parent)) };
        let reply = self.post(&suffix, json!({ "structuredQuery": query })).await?;
        Ok(reply.as_array().into_iter().flatten().filter_map(|item| item.get("document").cloned()).collect())
    }

    async fn count(&self, collection: &str, query: Value) -> Result<u64, DatabaseError> {
        let (parent, id) = split_collection(collection);
        let mut query = query;
        query["from"] = json!([{ "collectionId": id }]);
        let suffix = if parent.is_empty() { ":runAggregationQuery".to_string() } else { format!("/{}:runAggregationQuery", encode_path(parent)) };
        let body = json!({ "structuredAggregationQuery": { "structuredQuery": query, "aggregations": [{ "alias": "n", "count": {} }] } });
        let reply = self.post(&suffix, body).await?;
        reply[0]["result"]["aggregateFields"]["n"]["integerValue"]
            .as_str()
            .and_then(|count| count.parse().ok())
            .ok_or_else(|| DatabaseError::new("Firestore returned no count."))
    }

    /// Writes in one commit: all of them, or none.
    async fn commit(&self, writes: Vec<Value>) -> Result<(), DatabaseError> {
        if self.read_only {
            return Err(DatabaseError::new("This connection is read-only. Nothing was changed."));
        }
        for chunk in writes.chunks(500) {
            match self.post(":commit", json!({ "writes": chunk })).await {
                Err(DatabaseError::Failed { detail: Some(status), .. }) if status == "FAILED_PRECONDITION" || status == "NOT_FOUND" => {
                    return Err(DatabaseError::new(ROW_CHANGED));
                }
                Err(DatabaseError::Failed { detail: Some(status), .. }) if status == "ALREADY_EXISTS" => {
                    return Err(DatabaseError::new("A document with that ID already exists. Nothing was saved."));
                }
                other => {
                    other?;
                }
            }
        }
        Ok(())
    }

    pub(crate) async fn ping(&self) -> Result<(), DatabaseError> {
        self.collection_ids("").await.map(|_| ())
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
    }

    /// The full name of a document: projects/…/documents/users/ada.
    fn name(&self, collection: &str, id: &str) -> String {
        format!("{}/{collection}/{id}", self.root)
    }

    /// "users/ada" from a document's full name.
    fn path_of(&self, name: &str) -> String {
        name.strip_prefix(&self.root).map(|rest| rest.trim_start_matches('/').to_string()).unwrap_or_else(|| name.to_string())
    }
}

fn service_account(key: &Value) -> Result<ServiceAccount, DatabaseError> {
    let email = key["client_email"].as_str().ok_or_else(|| DatabaseError::new("The key has no client_email: is it a service account key?"))?;
    let pem = key["private_key"].as_str().ok_or_else(|| DatabaseError::new("The key has no private_key."))?;
    let body: String = pem.lines().filter(|line| !line.starts_with("-----")).collect();
    let der = base64::engine::general_purpose::STANDARD.decode(body.trim()).map_err(|_| DatabaseError::new("The key's private_key isn't readable."))?;
    let key_pair = ring::signature::RsaKeyPair::from_pkcs8(&der).map_err(|error| DatabaseError::new(format!("The key's private_key isn't an RSA key: {error}")))?;
    Ok(ServiceAccount {
        email: email.to_string(),
        key: key_pair,
        token_uri: key["token_uri"].as_str().unwrap_or("https://oauth2.googleapis.com/token").to_string(),
    })
}

/// "users/ada/orders" → ("users/ada", "orders").
fn split_collection(collection: &str) -> (&str, &str) {
    collection.rsplit_once('/').unwrap_or(("", collection))
}

/// Each segment percent-encoded, for the URL.
fn encode_path(path: &str) -> String {
    path.split('/')
        .map(|segment| {
            segment
                .bytes()
                .map(|byte| match byte {
                    b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (byte as char).to_string(),
                    _ => format!("%{byte:02X}"),
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// A field path as Firestore takes it: `name`, or `with space` in backticks.
fn field_path(name: &str) -> String {
    let simple = name.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if simple || name == ID {
        name.to_string()
    } else {
        format!("`{}`", name.replace('\\', "\\\\").replace('`', "\\`"))
    }
}

// MARK: - Values

/// The type of a Firestore value, as the Structure view and the grid's
/// typing name it.
fn type_name(value: &Value) -> &'static str {
    let Some((kind, _)) = value.as_object().and_then(|object| object.iter().next()) else { return "null" };
    match kind.as_str() {
        "booleanValue" => "boolean",
        "integerValue" => "integer",
        "doubleValue" => "double",
        "timestampValue" => "timestamp",
        "stringValue" => "string",
        "bytesValue" => "bytes",
        "referenceValue" => "reference",
        "geoPointValue" => "geopoint",
        "arrayValue" => "array",
        "mapValue" => "map",
        _ => "null",
    }
}

/// A value as the grid shows it: text plain, bytes as `\x…` hex, references
/// as their path, arrays and maps as JSON.
fn text(value: &Value) -> Option<String> {
    let object = value.as_object()?;
    let (kind, inner) = object.iter().next()?;
    Some(match kind.as_str() {
        "nullValue" => return None,
        "booleanValue" => inner.as_bool().unwrap_or_default().to_string(),
        "integerValue" => inner.as_str().map(str::to_string).unwrap_or_else(|| inner.to_string()),
        "doubleValue" => inner.as_f64().map(crate::sqlite::real).unwrap_or_else(|| inner.to_string()),
        "timestampValue" | "stringValue" => inner.as_str().unwrap_or_default().to_string(),
        "bytesValue" => {
            let bytes = base64::engine::general_purpose::STANDARD.decode(inner.as_str().unwrap_or_default()).unwrap_or_default();
            format!("\\x{}", bytes.iter().map(|byte| format!("{byte:02x}")).collect::<String>())
        }
        "referenceValue" => {
            let name = inner.as_str().unwrap_or_default();
            name.split_once("/documents/").map_or(name, |(_, path)| path).to_string()
        }
        "geoPointValue" => format!("{}, {}", inner["latitude"].as_f64().unwrap_or(0.0), inner["longitude"].as_f64().unwrap_or(0.0)),
        _ => plain(value).to_string(),
    })
}

/// A value as plain JSON: numbers as numbers, maps as objects.
fn plain(value: &Value) -> Value {
    let Some((kind, inner)) = value.as_object().and_then(|object| object.iter().next()) else { return Value::Null };
    match kind.as_str() {
        "booleanValue" => inner.clone(),
        "integerValue" => inner.as_str().and_then(|text| text.parse::<i64>().ok()).map_or(Value::Null, Value::from),
        "doubleValue" => inner.clone(),
        "arrayValue" => Value::Array(inner["values"].as_array().into_iter().flatten().map(plain).collect()),
        "mapValue" => Value::Object(inner["fields"].as_object().into_iter().flatten().map(|(key, value)| (key.clone(), plain(value))).collect()),
        "geoPointValue" => inner.clone(),
        "nullValue" => Value::Null,
        _ => text(value).map_or(Value::Null, Value::String),
    }
}

/// Plain JSON as a Firestore value: whole numbers as integers.
fn from_json(json: &Value) -> Value {
    match json {
        Value::Null => json!({ "nullValue": null }),
        Value::Bool(bit) => json!({ "booleanValue": bit }),
        Value::Number(number) => match number.as_i64() {
            Some(integer) => json!({ "integerValue": integer.to_string() }),
            None => json!({ "doubleValue": number.as_f64().unwrap_or(0.0) }),
        },
        Value::String(text) => json!({ "stringValue": text }),
        Value::Array(items) => json!({ "arrayValue": { "values": items.iter().map(from_json).collect::<Vec<_>>() } }),
        Value::Object(object) => {
            // The shell's Date("…") and ISODate("…") arrive as {"$date": "…"}.
            if let (1, Some(Value::String(date))) = (object.len(), object.get("$date")) {
                return json!({ "timestampValue": date });
            }
            json!({ "mapValue": { "fields": object.iter().map(|(key, value)| (key.clone(), from_json(value))).collect::<Map<_, _>>() } })
        }
    }
}

/// The grid's text as a value of the column's type: "36" stays an integer
/// in an integer field, objects and arrays are read as JS literals.
fn text_value(link: &FirestoreLink, text: &str, column_type: &str) -> Result<Value, DatabaseError> {
    let base = column_type.split(' ').next().unwrap_or_default();
    let refuse = |what: &str| DatabaseError::new(format!("“{text}” is not {what}."));
    let trimmed = text.trim();
    Ok(match base {
        "integer" => json!({ "integerValue": trimmed.parse::<i64>().map_err(|_| refuse("a whole number"))?.to_string() }),
        "double" => json!({ "doubleValue": trimmed.parse::<f64>().map_err(|_| refuse("a number"))? }),
        "boolean" => match trimmed.to_ascii_lowercase().as_str() {
            "true" => json!({ "booleanValue": true }),
            "false" => json!({ "booleanValue": false }),
            _ => return Err(refuse("true or false")),
        },
        "timestamp" => {
            let time = chrono::DateTime::parse_from_rfc3339(&trimmed.replacen(' ', "T", 1)).map_err(|_| refuse("a time such as 2024-01-02T03:04:05Z"))?;
            json!({ "timestampValue": time.to_utc().to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true) })
        }
        "bytes" => {
            let hex = trimmed.strip_prefix("\\x").ok_or_else(|| refuse("bytes written as \\x…"))?;
            let bytes = (0..hex.len())
                .step_by(2)
                .map(|start| hex.get(start..start + 2).and_then(|pair| u8::from_str_radix(pair, 16).ok()))
                .collect::<Option<Vec<u8>>>()
                .ok_or_else(|| refuse("bytes written as \\x…"))?;
            json!({ "bytesValue": base64::engine::general_purpose::STANDARD.encode(bytes) })
        }
        "reference" => json!({ "referenceValue": format!("{}/{}", link.root, trimmed.trim_matches('/')) }),
        "geopoint" => {
            let (latitude, longitude) = trimmed.split_once(',').ok_or_else(|| refuse("a point written as latitude, longitude"))?;
            let number = |text: &str| text.trim().parse::<f64>().map_err(|_| refuse("a point written as latitude, longitude"));
            json!({ "geoPointValue": { "latitude": number(latitude)?, "longitude": number(longitude)? } })
        }
        "array" | "map" => from_json(&literal(text)?),
        "string" => json!({ "stringValue": text }),
        // A field no sample showed: text, unless it is a literal with brackets.
        _ if trimmed.starts_with(['{', '[']) => from_json(&literal(text)?),
        _ => json!({ "stringValue": text }),
    })
}

/// A JS literal, as the shell takes it: unquoted keys, single quotes.
fn literal(text: &str) -> Result<Value, DatabaseError> {
    let mut parser = ShellParser { chars: text.chars().collect(), index: 0 };
    let value = parser.value()?;
    parser.space();
    if parser.index < parser.chars.len() {
        return Err(parser.error("Unexpected text after the value"));
    }
    Ok(value)
}

fn document_id(document: &Value) -> String {
    document["name"].as_str().and_then(|name| name.rsplit('/').next()).unwrap_or_default().to_string()
}

/// A document's cells, in `columns` order: the ID, then each field's text.
fn cells(document: &Value, columns: &[Column]) -> Vec<Option<String>> {
    columns
        .iter()
        .map(|column| if column.name == ID { Some(document_id(document)) } else { document["fields"].get(&column.name).and_then(text) })
        .collect()
}

// MARK: - Catalog and rows

impl DatabaseSession {
    fn firestore_link(&self) -> Result<FirestoreLink, DatabaseError> {
        self.firestore().ok_or_else(|| DatabaseError::new("This needs a Firestore connection."))
    }

    pub(crate) async fn firestore_schemas(&self) -> Result<Vec<SchemaContents>, DatabaseError> {
        let link = self.firestore_link()?;
        let ids = crate::on_runtime({
            let link = link.clone();
            async move { link.collection_ids("").await }
        })
        .await?;
        let relations = ids.into_iter().map(|name| Relation { schema: link.database.clone(), name, kind: RelationKind::Table }).collect();
        Ok(vec![SchemaContents { name: link.database, relations }])
    }

    /// The document ID, then the fields of a sample of documents, each with
    /// its most common type; "integer (mixed)" when others occur too.
    pub(crate) async fn firestore_columns(&self, relation: &Relation) -> Result<Vec<Column>, DatabaseError> {
        let link = self.firestore_link()?;
        let collection = relation.name.clone();
        let documents = crate::on_runtime(async move { link.run_query(&collection, json!({ "limit": SAMPLE })).await }).await?;
        let mut order: Vec<String> = Vec::new();
        let mut counts: std::collections::HashMap<String, std::collections::HashMap<&'static str, usize>> = Default::default();
        for document in &documents {
            for (name, value) in document["fields"].as_object().into_iter().flatten() {
                if !counts.contains_key(name) {
                    order.push(name.clone());
                }
                *counts.entry(name.clone()).or_default().entry(type_name(value)).or_default() += 1;
            }
        }
        order.sort();
        let mut columns = vec![Column { name: ID.into(), column_type: "id".into(), kind: ColumnKind::Other, enum_values: Vec::new(), is_numeric: false }];
        for name in order {
            let types = &counts[&name];
            let mut ranked: Vec<_> = types.iter().filter(|(kind, _)| **kind != "null").collect();
            ranked.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
            let main = ranked.first().map_or("null", |(kind, _)| **kind);
            let column_type = if ranked.len() > 1 { format!("{main} (mixed)") } else { main.to_string() };
            let kind = match main {
                "timestamp" => ColumnKind::TimestampTz,
                "boolean" => ColumnKind::Boolean,
                _ => ColumnKind::Other,
            };
            columns.push(Column { name, is_numeric: matches!(main, "integer" | "double"), column_type, kind, enum_values: Vec::new() });
        }
        Ok(columns)
    }

    pub(crate) async fn firestore_rows(
        &self,
        relation: Relation,
        columns: Vec<Column>,
        filters: Vec<RowFilter>,
        sort: Option<RowSort>,
        offset: u64,
        limit: u64,
    ) -> Result<RowPage, DatabaseError> {
        let link = self.firestore_link()?;
        let plan = QueryPlan::new(&link, &columns, &filters, sort.as_ref())?;
        crate::on_runtime(async move {
            let documents = plan.read(&link, &relation.name, offset, limit).await?;
            let rows = documents.iter().map(|document| cells(document, &columns).into_iter().map(|cell| cell.map(|text| crate::query::cut(&text))).collect()).collect();
            Ok(RowPage { rows, offset })
        })
        .await
    }

    pub(crate) async fn firestore_count(&self, relation: Relation, columns: Vec<Column>, filters: Vec<RowFilter>) -> Result<u64, DatabaseError> {
        let link = self.firestore_link()?;
        let plan = QueryPlan::new(&link, &columns, &filters, None)?;
        crate::on_runtime(async move {
            if plan.local.is_empty() {
                // A count needs no order, and an order can only narrow what Firestore accepts.
                let mut query = plan.query.clone();
                if let Some(object) = query.as_object_mut() {
                    object.remove("orderBy");
                }
                return link.count(&relation.name, query).await;
            }
            Ok(plan.read(&link, &relation.name, 0, u64::MAX).await?.len() as u64)
        })
        .await
    }

    pub(crate) async fn firestore_full_rows(&self, relation: Relation, columns: Vec<Column>, keys: Vec<Vec<String>>) -> Result<Vec<Vec<Option<String>>>, DatabaseError> {
        let link = self.firestore_link()?;
        let names: Vec<String> = keys.iter().filter_map(|key| key.first()).map(|id| link.name(&relation.name, id)).collect();
        crate::on_runtime(async move {
            let reply = link.post(":batchGet", json!({ "documents": names })).await?;
            let found: Vec<Value> = reply.as_array().into_iter().flatten().filter_map(|item| item.get("found").cloned()).collect();
            Ok(names
                .iter()
                .filter_map(|name| found.iter().find(|document| document["name"].as_str() == Some(name)))
                .map(|document| cells(document, &columns))
                .collect())
        })
        .await
    }

    pub(crate) async fn firestore_structure(&self, relation: Relation) -> Result<TableStructure, DatabaseError> {
        let columns = self.firestore_columns(&relation).await?;
        Ok(TableStructure {
            columns: columns
                .into_iter()
                .map(|column| ColumnDetail { nullable: column.name != ID, name: column.name, column_type: column.column_type, default_value: None, comment: None })
                .collect(),
            indexes: Vec::new(),
            constraints: Vec::new(),
        })
    }

    /// CSV or JSON of the matching documents; Firestore has no SQL to export as.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn firestore_export(
        &self,
        relation: Relation,
        columns: Vec<Column>,
        filters: Vec<RowFilter>,
        sort: Option<RowSort>,
        scope: crate::export::ExportScope,
        format: crate::export::ExportFormat,
        path: String,
    ) -> Result<u64, DatabaseError> {
        use crate::export::{ExportFormat, ExportScope};
        if format == ExportFormat::Sql {
            return Err(DatabaseError::new("Firestore has no SQL; export as JSON or CSV."));
        }
        let link = self.firestore_link()?;
        let plan = QueryPlan::new(&link, &columns, &filters, sort.as_ref())?;
        crate::on_runtime(async move {
            let documents: Vec<Value> = match scope {
                ExportScope::All => plan.read(&link, &relation.name, 0, u64::MAX).await?,
                ExportScope::Page { offset, limit } => plan.read(&link, &relation.name, offset, limit).await?,
                ExportScope::Keys { keys } => {
                    let names: Vec<String> = keys.iter().filter_map(|key| key.first()).map(|id| link.name(&relation.name, id)).collect();
                    let reply = link.post(":batchGet", json!({ "documents": names })).await?;
                    reply.as_array().into_iter().flatten().filter_map(|item| item.get("found").cloned()).collect()
                }
            };
            let mut out = String::new();
            match format {
                ExportFormat::Csv => {
                    out += &columns.iter().map(|column| crate::sqlite::csv_field(&column.name)).collect::<Vec<_>>().join(",");
                    out.push('\n');
                    for document in &documents {
                        out += &cells(document, &columns).iter().map(|cell| cell.as_deref().map(crate::sqlite::csv_field).unwrap_or_default()).collect::<Vec<_>>().join(",");
                        out.push('\n');
                    }
                }
                _ => {
                    let items: Vec<Value> = documents
                        .iter()
                        .map(|document| {
                            let mut object = Map::new();
                            object.insert(ID.into(), Value::String(document_id(document)));
                            for (name, value) in document["fields"].as_object().into_iter().flatten() {
                                object.insert(name.clone(), plain(value));
                            }
                            Value::Object(object)
                        })
                        .collect();
                    out = serde_json::to_string_pretty(&items).unwrap_or_default();
                    out.push('\n');
                }
            }
            std::fs::write(&path, out).map_err(|error| DatabaseError::new(format!("Can't write {path}: {error}.")))?;
            Ok(documents.len() as u64)
        })
        .await
    }
}

/// The structured query a page, count or export runs, and the filters left
/// for here: substring matches, which Firestore can't do, and "not equal",
/// which in Firestore leaves out documents without the field.
struct QueryPlan {
    query: Value,
    /// The field's name, or None for "any column".
    local: Vec<(Option<String>, FilterOperator, String)>,
}

impl QueryPlan {
    fn new(link: &FirestoreLink, columns: &[Column], filters: &[RowFilter], sort: Option<&RowSort>) -> Result<Self, DatabaseError> {
        let mut server = Vec::new();
        let mut local = Vec::new();
        // Firestore orders by a range-filtered field first; the order must say so.
        let mut ranged: Vec<String> = Vec::new();
        for filter in filters {
            let name = match &filter.target {
                FilterTarget::RawSql => return Err(DatabaseError::new("Firestore has no query text; use the filters, or the query editor.")),
                FilterTarget::AnyColumn => {
                    local.push((None, filter.operator, filter.value.clone()));
                    continue;
                }
                FilterTarget::Column { name } => name,
            };
            let position = columns.iter().position(|column| &column.name == name);
            let column_type = position.map(|index| columns[index].column_type.as_str()).unwrap_or("string");
            let value = |text: &str| -> Result<Value, DatabaseError> {
                if name == ID {
                    return Ok(json!({ "referenceValue": format!("{}/{}", link.root, text) }));
                }
                text_value(link, text, column_type)
            };
            let field = || json!({ "fieldPath": field_path(name) });
            let compare = |op: &str, text: &str| -> Result<Value, DatabaseError> {
                Ok(json!({ "fieldFilter": { "field": field(), "op": op, "value": value(text)? } }))
            };
            let list = || -> Result<Value, DatabaseError> {
                let items = filter.value.split(',').map(str::trim).filter(|item| !item.is_empty()).map(value).collect::<Result<Vec<_>, _>>()?;
                Ok(json!({ "arrayValue": { "values": items } }))
            };
            if matches!(filter.operator, FilterOperator::GreaterThan | FilterOperator::GreaterOrEqual | FilterOperator::LessThan | FilterOperator::LessOrEqual)
                || (filter.operator == FilterOperator::StartsWith && column_type.starts_with("string"))
            {
                if !ranged.contains(name) {
                    ranged.push(name.clone());
                }
            }
            match filter.operator {
                FilterOperator::Equals => server.push(compare("EQUAL", &filter.value)?),
                FilterOperator::GreaterThan => server.push(compare("GREATER_THAN", &filter.value)?),
                FilterOperator::GreaterOrEqual => server.push(compare("GREATER_THAN_OR_EQUAL", &filter.value)?),
                FilterOperator::LessThan => server.push(compare("LESS_THAN", &filter.value)?),
                FilterOperator::LessOrEqual => server.push(compare("LESS_THAN_OR_EQUAL", &filter.value)?),
                FilterOperator::In => server.push(json!({ "fieldFilter": { "field": field(), "op": "IN", "value": list()? } })),
                FilterOperator::IsNull => server.push(json!({ "unaryFilter": { "field": field(), "op": "IS_NULL" } })),
                FilterOperator::StartsWith if column_type.starts_with("string") => {
                    server.push(json!({ "fieldFilter": { "field": field(), "op": "GREATER_THAN_OR_EQUAL", "value": { "stringValue": filter.value } } }));
                    server.push(json!({ "fieldFilter": { "field": field(), "op": "LESS_THAN", "value": { "stringValue": format!("{}\u{f8ff}", filter.value) } } }));
                }
                _ => local.push((Some(name.clone()), filter.operator, filter.value.clone())),
            }
        }
        let mut query = Map::new();
        match server.len() {
            0 => {}
            1 => {
                query.insert("where".into(), server.remove(0));
            }
            _ => {
                query.insert("where".into(), json!({ "compositeFilter": { "op": "AND", "filters": server } }));
            }
        }
        let mut order = Vec::new();
        if let Some(sort) = sort {
            order.push(json!({ "field": { "fieldPath": field_path(&sort.column) }, "direction": if sort.ascending { "ASCENDING" } else { "DESCENDING" } }));
        }
        for name in ranged.iter().filter(|name| sort.is_none_or(|sort| &&sort.column != name) && name.as_str() != ID) {
            order.push(json!({ "field": { "fieldPath": field_path(name) }, "direction": "ASCENDING" }));
        }
        if sort.is_none_or(|sort| sort.column != ID) {
            order.push(json!({ "field": { "fieldPath": ID }, "direction": "ASCENDING" }));
        }
        query.insert("orderBy".into(), Value::Array(order));
        Ok(Self { query: Value::Object(query), local })
    }

    /// `limit` matching documents after `offset`: with an offset on the
    /// server when it filters alone, else read on with a cursor and filtered here.
    async fn read(&self, link: &FirestoreLink, collection: &str, offset: u64, limit: u64) -> Result<Vec<Value>, DatabaseError> {
        if self.local.is_empty() {
            let mut query = self.query.clone();
            if offset > 0 {
                query["offset"] = json!(offset);
            }
            if limit != u64::MAX {
                query["limit"] = json!(limit);
                return link.run_query(collection, query).await;
            }
        }
        let mut kept = Vec::new();
        let mut skipped = 0u64;
        let mut cursor: Option<Value> = None;
        loop {
            let mut query = self.query.clone();
            query["limit"] = json!(SCAN_PAGE);
            if self.local.is_empty() && offset > 0 && cursor.is_none() {
                query["offset"] = json!(offset);
                skipped = offset;
            }
            if let Some(cursor) = &cursor {
                query["startAt"] = json!({ "values": cursor, "before": false });
            }
            let documents = link.run_query(collection, query).await?;
            let Some(last) = documents.last() else { break };
            cursor = Some(self.cursor_values(last));
            let done = (documents.len() as u64) < SCAN_PAGE;
            for document in documents {
                if !self.matches(&document) {
                    continue;
                }
                if skipped < offset {
                    skipped += 1;
                    continue;
                }
                kept.push(document);
                if kept.len() as u64 >= limit {
                    return Ok(kept);
                }
            }
            if done {
                break;
            }
        }
        Ok(kept)
    }

    /// The values of the orderBy fields of `document`, to start after it.
    fn cursor_values(&self, document: &Value) -> Value {
        Value::Array(
            self.query["orderBy"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|order| {
                    let path = order["field"]["fieldPath"].as_str().unwrap_or(ID);
                    if path == ID {
                        json!({ "referenceValue": document["name"] })
                    } else {
                        document["fields"].get(path.trim_matches('`')).cloned().unwrap_or(json!({ "nullValue": null }))
                    }
                })
                .collect(),
        )
    }

    fn matches(&self, document: &Value) -> bool {
        let cell = |name: &str| if name == ID { Some(document_id(document)) } else { document["fields"].get(name).and_then(text) };
        self.local.iter().all(|(name, operator, value)| match name {
            Some(name) => crate::cassandra::compare(cell(name).as_deref(), *operator, value),
            None => std::iter::once(Some(document_id(document)))
                .chain(document["fields"].as_object().into_iter().flatten().map(|(_, field)| text(field)))
                .any(|cell| cell.is_some() && crate::cassandra::compare(cell.as_deref(), *operator, value)),
        })
    }
}

// MARK: - Changes

impl DatabaseSession {
    /// Applies the changes in one commit. Each edited or deleted document
    /// must still exist and each new one must not, or nothing is saved.
    pub(crate) async fn firestore_commit(&self, relation: Relation, changes: PendingChanges, columns: Vec<Column>) -> Result<(), DatabaseError> {
        let link = self.firestore_link()?;
        let writes = writes(&link, &relation, &changes, &columns)?;
        crate::on_runtime(async move { link.commit(writes).await }).await
    }
}

fn column_type<'a>(columns: &'a [Column], name: &str) -> &'a str {
    columns.iter().find(|column| column.name == name).map_or("", |column| column.column_type.as_str())
}

/// A draft as a Firestore value; None removes the field, as NULL does for MongoDB.
fn draft_value(link: &FirestoreLink, draft: &CellDraft, column_type: &str) -> Result<Option<Value>, DatabaseError> {
    Ok(match draft {
        CellDraft::Value { text } => Some(text_value(link, text, column_type)?),
        CellDraft::Null => None,
        CellDraft::Default => return Err(DatabaseError::new("Firestore fields have no defaults. Type the value instead.")),
        CellDraft::Function { function: SqlFunction::Now } => {
            Some(json!({ "timestampValue": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true) }))
        }
        CellDraft::Function { .. } => return Err(DatabaseError::new("Firestore has only full timestamps; choose NOW().")),
    })
}

fn writes(link: &FirestoreLink, relation: &Relation, changes: &PendingChanges, columns: &[Column]) -> Result<Vec<Value>, DatabaseError> {
    let mut writes = Vec::new();
    for update in &changes.updates {
        let id = update.key.first().ok_or_else(|| DatabaseError::new("The document has no ID."))?;
        if update.values.contains_key(ID) {
            return Err(DatabaseError::new("A document's ID can't change; add a new document and delete this one."));
        }
        let mut fields = Map::new();
        let mut mask = Vec::new();
        for (name, draft) in &update.values {
            mask.push(field_path(name));
            if let Some(value) = draft_value(link, draft, column_type(columns, name))? {
                fields.insert(name.clone(), value);
            }
        }
        mask.sort();
        writes.push(json!({
            "update": { "name": link.name(&relation.name, id), "fields": fields },
            "updateMask": { "fieldPaths": mask },
            "currentDocument": { "exists": true },
        }));
    }
    for key in &changes.deletes {
        let id = key.first().ok_or_else(|| DatabaseError::new("The document has no ID."))?;
        writes.push(json!({ "delete": link.name(&relation.name, id), "currentDocument": { "exists": true } }));
    }
    for row in &changes.inserts {
        writes.push(insert_write(link, relation, row, columns)?);
    }
    Ok(writes)
}

fn insert_write(link: &FirestoreLink, relation: &Relation, row: &NewRow, columns: &[Column]) -> Result<Value, DatabaseError> {
    let id = match row.values.get(ID) {
        Some(CellDraft::Value { text }) if !text.trim().is_empty() => text.trim().to_string(),
        _ => auto_id(),
    };
    let mut fields = Map::new();
    for (name, draft) in &row.values {
        if name == ID {
            continue;
        }
        if let Some(value) = draft_value(link, draft, column_type(columns, name))? {
            fields.insert(name.clone(), value);
        }
    }
    Ok(json!({ "update": { "name": link.name(&relation.name, &id), "fields": fields }, "currentDocument": { "exists": false } }))
}

/// A random 20-character ID, as the SDKs make for add().
fn auto_id() -> String {
    use ring::rand::SecureRandom;
    const LETTERS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut bytes = [0u8; 20];
    if ring::rand::SystemRandom::new().fill(&mut bytes).is_err() {
        let seed = SystemTime::now().duration_since(UNIX_EPOCH).map(|elapsed| elapsed.as_nanos()).unwrap_or(0);
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = (seed >> (index % 16 * 8)) as u8 ^ index as u8;
        }
    }
    bytes.iter().map(|byte| LETTERS[*byte as usize % LETTERS.len()] as char).collect()
}

/// A Firestore value as a JS literal, for Review.
fn js(value: &Value) -> String {
    match type_name(value) {
        "timestamp" => format!("new Date({})", serde_json::to_string(&text(value).unwrap_or_default()).unwrap_or_default()),
        "reference" => format!("db.doc({})", serde_json::to_string(&text(value).unwrap_or_default()).unwrap_or_default()),
        _ => plain(value).to_string(),
    }
}

/// The changes as the Firebase SDK writes them, for Review.
pub(crate) fn change_commands(changes: &PendingChanges, relation: &Relation, columns: &[Column]) -> Result<Vec<String>, DatabaseError> {
    // Review needs no project: the values only need their types.
    let link = FirestoreLink {
        client: reqwest::Client::new(),
        base: String::new(),
        root: String::new(),
        account: None,
        token: Arc::new(Mutex::new(None)),
        version: String::new(),
        database: String::new(),
        read_only: false,
        endpoint: String::new(),
        closed: Arc::new(AtomicBool::new(false)),
    };
    let collection = format!("db.collection({})", serde_json::to_string(&relation.name).unwrap_or_default());
    // The fields, and FieldValue.delete() for each masked field left out.
    let fields_text = |write: &Value| -> String {
        let quote = |key: &str| serde_json::to_string(key).unwrap_or_default();
        let fields = write["update"]["fields"].as_object().cloned().unwrap_or_default();
        let mut entries: Vec<String> = fields.iter().map(|(key, value)| format!("{}: {}", quote(key), js(value))).collect();
        for path in write["updateMask"]["fieldPaths"].as_array().into_iter().flatten().filter_map(Value::as_str) {
            let key = path.trim_matches('`');
            if !fields.contains_key(key) {
                entries.push(format!("{}: FieldValue.delete()", quote(key)));
            }
        }
        format!("{{ {} }}", entries.join(", "))
    };
    let mut lines = Vec::new();
    for write in writes(&link, relation, changes, columns)? {
        let id = |name: &str| serde_json::to_string(name.rsplit('/').next().unwrap_or_default()).unwrap_or_default();
        if let Some(name) = write["delete"].as_str() {
            lines.push(format!("{collection}.doc({}).delete()", id(name)));
        } else if write.get("updateMask").is_some() {
            let name = write["update"]["name"].as_str().unwrap_or_default();
            lines.push(format!("{collection}.doc({}).update({})", id(name), fields_text(&write)));
        } else {
            let name = write["update"]["name"].as_str().unwrap_or_default();
            lines.push(format!("{collection}.doc({}).create({})", id(name), fields_text(&write)));
        }
    }
    Ok(lines)
}

// MARK: - The query editor

/// One SDK chain: db.collection(…)… or db.doc(…)…
#[derive(Debug, Default)]
struct Chain {
    /// "users", "users/ada/orders"; a document path for doc chains.
    path: String,
    group: bool,
    is_document: bool,
    wheres: Vec<(String, String, Value)>,
    order: Vec<(String, bool)>,
    limit: Option<u64>,
    offset: Option<u64>,
    /// get, count, set, update, delete, add, create, listCollections, recursiveDelete.
    action: String,
    argument: Option<Value>,
}

const READ_ACTIONS: &[&str] = &["get", "count", "listCollections"];

fn parse_chains(text: &str) -> Result<Vec<Chain>, DatabaseError> {
    let mut parser = ShellParser { chars: text.chars().collect(), index: 0 };
    let mut chains = Vec::new();
    loop {
        parser.space();
        while parser.eat(';') {}
        parser.space();
        if parser.index >= parser.chars.len() {
            break;
        }
        let root = parser.word();
        if root != "db" && !root.starts_with("db.") {
            return Err(parser.error("A statement starts with db."));
        }
        let mut chain = Chain::default();
        // word() reads dots too: "db.collection" arrives whole.
        let mut pending: Vec<String> = root.split('.').skip(1).map(str::to_string).collect();
        loop {
            let method = if pending.is_empty() {
                if !parser.eat('.') {
                    break;
                }
                let word = parser.word();
                pending = word.split('.').map(str::to_string).collect();
                pending.remove(0)
            } else {
                pending.remove(0)
            };
            parser.expect('(')?;
            let mut arguments = Vec::new();
            if !parser.eat(')') {
                loop {
                    parser.space();
                    // db.collection(…) as an argument, as recursiveDelete takes.
                    if parser.chars[parser.index..].starts_with(&['d', 'b', '.']) {
                        let inner_start = parser.index;
                        let mut depth = 0;
                        while let Some(character) = parser.peek() {
                            match character {
                                '(' => depth += 1,
                                ')' if depth == 0 => break,
                                ')' => depth -= 1,
                                ',' if depth == 0 => break,
                                _ => {}
                            }
                            parser.index += 1;
                        }
                        let inner: String = parser.chars[inner_start..parser.index].iter().collect();
                        let mut nested = parse_chains(&inner)?;
                        arguments.push(json!({ "$chain": nested.pop().map(|chain| chain.path).unwrap_or_default() }));
                    } else {
                        arguments.push(parser.value()?);
                    }
                    if !parser.eat(',') {
                        parser.expect(')')?;
                        break;
                    }
                }
            }
            let text_argument = |index: usize| -> Result<String, DatabaseError> {
                arguments.get(index).and_then(Value::as_str).map(str::to_string).ok_or_else(|| DatabaseError::new(format!("{method}() takes a name in quotes.")))
            };
            match method.as_str() {
                "collection" => {
                    let name = text_argument(0)?;
                    chain.path = if chain.path.is_empty() { name } else { format!("{}/{name}", chain.path) };
                    chain.is_document = false;
                }
                "collectionGroup" => {
                    chain.path = text_argument(0)?;
                    chain.group = true;
                }
                "doc" => {
                    let name = text_argument(0)?;
                    chain.path = if chain.path.is_empty() { name } else { format!("{}/{name}", chain.path) };
                    chain.is_document = true;
                }
                "where" => {
                    let field = text_argument(0)?;
                    let op = text_argument(1)?;
                    chain.wheres.push((field, op, arguments.get(2).cloned().unwrap_or(Value::Null)));
                }
                "orderBy" => {
                    let field = text_argument(0)?;
                    let descending = arguments.get(1).and_then(Value::as_str).is_some_and(|direction| direction.eq_ignore_ascii_case("desc"));
                    chain.order.push((field, !descending));
                }
                "limit" => chain.limit = arguments.first().and_then(Value::as_u64),
                "offset" => chain.offset = arguments.first().and_then(Value::as_u64),
                "get" | "count" | "delete" | "listCollections" => chain.action = method.clone(),
                "set" | "update" | "add" | "create" => {
                    chain.action = method.clone();
                    chain.argument = Some(arguments.first().cloned().ok_or_else(|| DatabaseError::new(format!("{method}() takes the fields.")))?);
                }
                "recursiveDelete" => {
                    chain.action = method.clone();
                    chain.path = arguments.first().and_then(|argument| argument["$chain"].as_str()).map(str::to_string).ok_or_else(|| DatabaseError::new("recursiveDelete() takes db.collection(…) or db.doc(…)."))?;
                    chain.is_document = chain.path.split('/').count() % 2 == 0;
                }
                other => return Err(DatabaseError::new(format!("TablePort's Firestore editor doesn't know {other}(). It knows collection, collectionGroup, doc, where, orderBy, limit, offset, get, count, set, update, add, create, delete, listCollections and recursiveDelete."))),
            }
            if !chain.action.is_empty() {
                break;
            }
        }
        if chain.action.is_empty() {
            // A chain without an action reads, as the console does.
            chain.action = if chain.path.is_empty() { "listCollections".into() } else { "get".into() };
        }
        chains.push(chain);
    }
    Ok(chains)
}

fn operator(op: &str) -> Result<&'static str, DatabaseError> {
    Ok(match op {
        "==" => "EQUAL",
        "!=" => "NOT_EQUAL",
        "<" => "LESS_THAN",
        "<=" => "LESS_THAN_OR_EQUAL",
        ">" => "GREATER_THAN",
        ">=" => "GREATER_THAN_OR_EQUAL",
        "array-contains" => "ARRAY_CONTAINS",
        "array-contains-any" => "ARRAY_CONTAINS_ANY",
        "in" => "IN",
        "not-in" => "NOT_IN",
        other => return Err(DatabaseError::new(format!("Firestore has no operator {other}."))),
    })
}

impl FirestoreLink {
    fn chain_query(&self, chain: &Chain) -> Result<Value, DatabaseError> {
        let mut query = Map::new();
        let filters = chain
            .wheres
            .iter()
            .map(|(field, op, value)| {
                let value = if field == ID { json!({ "referenceValue": format!("{}/{}/{}", self.root, chain.path, value.as_str().unwrap_or_default()) }) } else { from_json(value) };
                Ok(json!({ "fieldFilter": { "field": { "fieldPath": field_path(field) }, "op": operator(op)?, "value": value } }))
            })
            .collect::<Result<Vec<_>, DatabaseError>>()?;
        match filters.len() {
            0 => {}
            1 => {
                query.insert("where".into(), filters.into_iter().next().unwrap_or_default());
            }
            _ => {
                query.insert("where".into(), json!({ "compositeFilter": { "op": "AND", "filters": filters } }));
            }
        }
        if !chain.order.is_empty() {
            query.insert(
                "orderBy".into(),
                Value::Array(chain.order.iter().map(|(field, ascending)| json!({ "field": { "fieldPath": field_path(field) }, "direction": if *ascending { "ASCENDING" } else { "DESCENDING" } })).collect()),
            );
        }
        if let Some(limit) = chain.limit {
            query.insert("limit".into(), json!(limit));
        }
        if let Some(offset) = chain.offset {
            query.insert("offset".into(), json!(offset));
        }
        Ok(Value::Object(query))
    }

    async fn query_chain(&self, chain: &Chain, query: Value) -> Result<Vec<Value>, DatabaseError> {
        if !chain.group {
            return self.run_query(&chain.path, query).await;
        }
        let mut query = query;
        query["from"] = json!([{ "collectionId": chain.path, "allDescendants": true }]);
        let reply = self.post(":runQuery", json!({ "structuredQuery": query })).await?;
        Ok(reply.as_array().into_iter().flatten().filter_map(|item| item.get("document").cloned()).collect())
    }

    /// Runs one chain: a result for reads, a count of written documents for writes.
    async fn run_chain(&self, chain: &Chain, max_rows: u64) -> Result<(Option<crate::query::ResultSet>, u64), DatabaseError> {
        let documents_result = |documents: Vec<Value>, total: u64, link: &FirestoreLink| {
            let mut names: Vec<String> = vec![ID.to_string()];
            for document in &documents {
                for key in document["fields"].as_object().into_iter().flatten().map(|(key, _)| key) {
                    if !names.contains(key) {
                        names.push(key.clone());
                    }
                }
            }
            let numeric = names
                .iter()
                .map(|name| documents.iter().filter_map(|document| document["fields"].get(name)).find(|value| type_name(value) != "null").is_some_and(|value| matches!(type_name(value), "integer" | "double")))
                .collect();
            let rows = documents
                .iter()
                .take(usize::try_from(max_rows).unwrap_or(usize::MAX))
                .map(|document| {
                    names
                        .iter()
                        .map(|name| {
                            if name == ID {
                                // The path tells subcollection documents apart.
                                Some(if chain.group { link.path_of(document["name"].as_str().unwrap_or_default()) } else { document_id(document) })
                            } else {
                                document["fields"].get(name).and_then(text).map(|text| crate::query::cut(&text))
                            }
                        })
                        .collect()
                })
                .collect();
            crate::query::ResultSet { columns: names, rows, total_rows: total, numeric_columns: numeric }
        };
        match chain.action.as_str() {
            "listCollections" => {
                let ids = self.collection_ids(&chain.path).await?;
                let total = ids.len() as u64;
                let rows = ids.into_iter().map(|id| vec![Some(id)]).collect();
                Ok((Some(crate::query::ResultSet { columns: vec!["collection".into()], rows, total_rows: total, numeric_columns: vec![false] }), total))
            }
            "get" if chain.is_document => {
                let reply = self.post(":batchGet", json!({ "documents": [format!("{}/{}", self.root, chain.path)] })).await?;
                let documents: Vec<Value> = reply.as_array().into_iter().flatten().filter_map(|item| item.get("found").cloned()).collect();
                let total = documents.len() as u64;
                Ok((Some(documents_result(documents, total, self)), total))
            }
            "get" => {
                let documents = self.query_chain(chain, self.chain_query(chain)?).await?;
                let total = documents.len() as u64;
                Ok((Some(documents_result(documents, total, self)), total))
            }
            "count" => {
                let count = if chain.group {
                    let mut query = self.chain_query(chain)?;
                    query["from"] = json!([{ "collectionId": chain.path, "allDescendants": true }]);
                    let body = json!({ "structuredAggregationQuery": { "structuredQuery": query, "aggregations": [{ "alias": "n", "count": {} }] } });
                    let reply = self.post(":runAggregationQuery", body).await?;
                    reply[0]["result"]["aggregateFields"]["n"]["integerValue"].as_str().and_then(|count| count.parse().ok()).unwrap_or(0)
                } else {
                    self.count(&chain.path, self.chain_query(chain)?).await?
                };
                let rows = vec![vec![Some(count.to_string())]];
                Ok((Some(crate::query::ResultSet { columns: vec!["count".into()], rows, total_rows: 1, numeric_columns: vec![true] }), count))
            }
            "recursiveDelete" => {
                let deleted = self.recursive_delete(&chain.path, chain.is_document).await?;
                Ok((None, deleted))
            }
            action => {
                let name = |path: &str| format!("{}/{path}", self.root);
                let fields = |value: &Option<Value>| -> Result<Map<String, Value>, DatabaseError> {
                    let object = value.as_ref().and_then(Value::as_object).ok_or_else(|| DatabaseError::new(format!("{action}() takes an object of fields.")))?;
                    Ok(object.iter().map(|(key, value)| (key.clone(), from_json(value))).collect())
                };
                let write = match (action, chain.is_document) {
                    ("set", true) => json!({ "update": { "name": name(&chain.path), "fields": fields(&chain.argument)? } }),
                    ("create", true) => json!({ "update": { "name": name(&chain.path), "fields": fields(&chain.argument)? }, "currentDocument": { "exists": false } }),
                    ("update", true) => {
                        let fields = fields(&chain.argument)?;
                        let mask: Vec<String> = fields.keys().map(|key| field_path(key)).collect();
                        json!({ "update": { "name": name(&chain.path), "fields": fields }, "updateMask": { "fieldPaths": mask }, "currentDocument": { "exists": true } })
                    }
                    ("delete", true) => json!({ "delete": name(&chain.path) }),
                    ("add", false) => json!({ "update": { "name": name(&format!("{}/{}", chain.path, auto_id())), "fields": fields(&chain.argument)? }, "currentDocument": { "exists": false } }),
                    (action, _) => return Err(DatabaseError::new(format!("{action}() works on {}.", if chain.is_document { "a collection" } else { "a document: db.collection(…).doc(…)" }))),
                };
                self.commit(vec![write]).await?;
                Ok((None, 1))
            }
        }
    }

    /// Deletes a collection's or a document's documents, and theirs under
    /// them, in commits of up to 500.
    async fn recursive_delete(&self, path: &str, is_document: bool) -> Result<u64, DatabaseError> {
        if self.read_only {
            return Err(DatabaseError::new("This connection is read-only. Nothing was changed."));
        }
        let mut deleted = 0u64;
        let mut pending: Vec<(String, bool)> = vec![(path.to_string(), is_document)];
        while let Some((path, is_document)) = pending.pop() {
            if is_document {
                for collection in self.collection_ids(&path).await? {
                    pending.push((format!("{path}/{collection}"), false));
                }
                self.commit(vec![json!({ "delete": format!("{}/{path}", self.root) })]).await?;
                deleted += 1;
                continue;
            }
            loop {
                let documents = self.run_query(&path, json!({ "limit": 500 })).await?;
                if documents.is_empty() {
                    break;
                }
                let mut writes = Vec::new();
                for document in &documents {
                    let name = document["name"].as_str().unwrap_or_default().to_string();
                    for collection in self.collection_ids(&self.path_of(&name)).await? {
                        pending.push((format!("{}/{collection}", self.path_of(&name)), false));
                    }
                    writes.push(json!({ "delete": name }));
                }
                deleted += writes.len() as u64;
                self.commit(writes).await?;
            }
        }
        Ok(deleted)
    }
}

impl DatabaseSession {
    /// The query editor's Run: each chain in turn. Writes commit one by
    /// one; Firestore has no transaction a script could span here.
    pub(crate) async fn firestore_run(&self, text: String, max_rows: u64, read_only: bool) -> Result<crate::query::QueryResult, DatabaseError> {
        let link = self.firestore_link()?;
        let chains = parse_chains(&text)?;
        if read_only || link.read_only {
            if let Some(chain) = chains.iter().find(|chain| !READ_ACTIONS.contains(&chain.action.as_str())) {
                return Err(DatabaseError::new(format!("Only reads may run here: {}() changes data.", chain.action)));
            }
        }
        crate::on_runtime(async move {
            let start = Instant::now();
            let mut result_sets = Vec::new();
            let mut counts = Vec::new();
            let mut verbs = Vec::new();
            for chain in &chains {
                let (set, count) = link.run_chain(chain, max_rows).await?;
                verbs.push(chain.action.to_uppercase());
                counts.push(count);
                result_sets.extend(set);
            }
            Ok(crate::query::QueryResult { result_sets, statement_counts: counts, statement_verbs: verbs, elapsed_ms: start.elapsed().as_millis().try_into().unwrap_or(u64::MAX) })
        })
        .await
    }

    /// One read for AI access: get, count or listCollections, as rows of JSON.
    pub(crate) async fn firestore_read(&self, text: String, max_rows: u64) -> Result<Vec<Value>, DatabaseError> {
        let chains = parse_chains(&text)?;
        if chains.len() != 1 {
            return Err(DatabaseError::new("Send one call at a time."));
        }
        let result = self.firestore_run(text, max_rows, true).await?;
        let Some(set) = result.result_sets.into_iter().next() else { return Ok(Vec::new()) };
        Ok(set
            .rows
            .into_iter()
            .map(|row| Value::Object(set.columns.iter().cloned().zip(row.into_iter().map(|cell| cell.map_or(Value::Null, Value::String))).collect()))
            .collect())
    }
}

/// The SDK call that empties or removes a collection, for Drop and Truncate:
/// Firestore deletes a collection by deleting its documents.
pub(crate) fn recursive_delete_command(relation: &Relation) -> String {
    format!("db.recursiveDelete(db.collection({}))", serde_json::to_string(&relation.name).unwrap_or_default())
}

pub(crate) fn insert_command(row: &NewRow, relation: &Relation, columns: &[Column]) -> Result<String, DatabaseError> {
    let changes = PendingChanges { inserts: vec![row.clone()], ..Default::default() };
    Ok(change_commands(&changes, relation, columns)?.into_iter().next().unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sdk_chains() {
        let chains = parse_chains("db.collection('users').where('age', '>=', 30).orderBy('name', 'desc').limit(5).get();\ndb.collection(\"users\").doc('ada').update({ age: 37, tags: ['x'] })").unwrap();
        assert_eq!(chains.len(), 2);
        assert_eq!(chains[0].path, "users");
        assert_eq!(chains[0].wheres[0].0, "age");
        assert_eq!(chains[0].order, [("name".to_string(), false)]);
        assert_eq!(chains[0].limit, Some(5));
        assert_eq!(chains[1].path, "users/ada");
        assert!(chains[1].is_document);
        assert_eq!(chains[1].action, "update");
        let delete = parse_chains("db.recursiveDelete(db.collection('logs'))").unwrap();
        assert_eq!((delete[0].action.as_str(), delete[0].path.as_str(), delete[0].is_document), ("recursiveDelete", "logs", false));
        assert_eq!(parse_chains("db.collection('a').doc('b').collection('c')").unwrap()[0].path, "a/b/c");
        assert!(parse_chains("db.collection('a').drop()").is_err());
    }

    #[test]
    fn converts_values() {
        assert_eq!(from_json(&json!(36)), json!({ "integerValue": "36" }));
        assert_eq!(from_json(&json!(9.5)), json!({ "doubleValue": 9.5 }));
        assert_eq!(text(&json!({ "bytesValue": "yv4=" })).as_deref(), Some("\\xcafe"));
        assert_eq!(text(&json!({ "mapValue": { "fields": { "a": { "integerValue": "1" } } } })).as_deref(), Some("{\"a\":1}"));
        assert_eq!(field_path("first name"), "`first name`");
        assert_eq!(field_path("age"), "age");
        assert_eq!(auto_id().len(), 20);
    }
}
