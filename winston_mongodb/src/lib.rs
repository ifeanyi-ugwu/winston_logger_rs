//! MongoDB transport for winston.
//!
//! # Runtime requirement
//!
//! The `mongodb` crate uses tokio internally. The Logger that owns this
//! transport therefore needs a tokio-aware spawner — see [`tokio_spawner`]
//! for a ready-made one. Construct the Logger with
//! `Logger::new_with_spawner(opts, tokio_spawner())` so the per-transport
//! `WritableStream` task runs inside a tokio runtime.

mod to_mongodb_filter;

use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::Arc,
};

use chrono::{DateTime, Utc};
use futures::StreamExt;
use logform::LogInfo;
use mongodb::{
    bson::{self, doc, Document},
    options::{FindOptions, IndexOptions},
    Client, Collection, IndexModel,
};
use serde::{Deserialize, Serialize};
use to_mongodb_filter::ToMongoDbFilter;
use whatwg_streams::{
    ReadableSource, ReadableStreamDefaultController, StreamError, StreamResult, WritableSink,
    WritableStreamDefaultController,
};
use winston_transport::{DynQueryHandle, DynReadableSource, LogQuery, Order, Transport};

#[derive(Debug, Serialize, Deserialize)]
struct LogDocument {
    #[serde(with = "bson::serde_helpers::chrono_datetime_as_bson_datetime")]
    timestamp: DateTime<Utc>,
    level: String,
    message: String,
    #[serde(flatten)]
    meta: HashMap<String, serde_json::Value>,
}

#[derive(Clone, Debug)]
pub struct MongoDBOptions {
    pub connection_string: String,
    pub database: String,
    pub collection: String,
}

/// MongoDB-backed `Transport`.
///
/// `start()` opens a client and ensures the standard indexes exist; subsequent
/// `write()` calls insert one document per log entry. `query_handle()` returns
/// a handle that opens a fresh source per query — see [`MongoDBQueryHandle`].
pub struct MongoDBTransport {
    options: MongoDBOptions,
    /// Filled in `start`. None until then; `write` errors if called before
    /// `start` (which only happens if the WritableStream skipped `start`).
    collection: Option<Collection<LogDocument>>,
}

impl MongoDBTransport {
    pub fn new(options: MongoDBOptions) -> Self {
        Self {
            options,
            collection: None,
        }
    }

    pub fn builder(
        connection_string: impl Into<String>,
        database: impl Into<String>,
        collection: impl Into<String>,
    ) -> MongoDBTransportBuilder {
        MongoDBTransportBuilder {
            options: MongoDBOptions {
                connection_string: connection_string.into(),
                database: database.into(),
                collection: collection.into(),
            },
        }
    }
}

pub struct MongoDBTransportBuilder {
    options: MongoDBOptions,
}

impl MongoDBTransportBuilder {
    pub fn build(self) -> MongoDBTransport {
        MongoDBTransport::new(self.options)
    }
}

impl WritableSink<LogInfo> for MongoDBTransport {
    async fn start(
        &mut self,
        _controller: &mut WritableStreamDefaultController,
    ) -> StreamResult<()> {
        let client = Client::with_uri_str(&self.options.connection_string)
            .await
            .map_err(StreamError::other)?;
        let db = client.database(&self.options.database);
        let collection: Collection<LogDocument> = db.collection(&self.options.collection);
        create_indexes(&collection).await.map_err(StreamError::other)?;
        self.collection = Some(collection);
        Ok(())
    }

    async fn write(
        &mut self,
        info: LogInfo,
        _controller: &mut WritableStreamDefaultController,
    ) -> StreamResult<()> {
        let collection = self
            .collection
            .as_ref()
            .ok_or_else(|| StreamError::from("MongoDBTransport not started"))?;
        let doc = LogDocument {
            timestamp: Utc::now(),
            level: info.level,
            message: info.message,
            meta: info.meta,
        };
        collection
            .insert_one(doc)
            .await
            .map_err(StreamError::other)?;
        Ok(())
    }
}

impl Transport for MongoDBTransport {
    fn query_handle(&self) -> Option<Box<dyn DynQueryHandle>> {
        Some(Box::new(MongoDBQueryHandle {
            options: self.options.clone(),
        }))
    }
}

/// Long-lived handle the Logger keeps after `MongoDBTransport` itself is
/// consumed by the `WritableStream`. Opens a fresh `MongoDBSource` per query.
pub struct MongoDBQueryHandle {
    options: MongoDBOptions,
}

impl DynQueryHandle for MongoDBQueryHandle {
    fn query(&self, options: &LogQuery) -> Option<Box<dyn DynReadableSource>> {
        Some(Box::new(MongoDBSource::new(
            self.options.clone(),
            options.clone(),
        )))
    }
}

/// Streaming source: opens client + cursor lazily on first `pull`, then
/// emits one document per `pull` call until the cursor is exhausted.
pub struct MongoDBSource {
    options: MongoDBOptions,
    query: LogQuery,
    cursor: Option<mongodb::Cursor<LogDocument>>,
    initialized: bool,
}

impl MongoDBSource {
    fn new(options: MongoDBOptions, query: LogQuery) -> Self {
        Self {
            options,
            query,
            cursor: None,
            initialized: false,
        }
    }

    async fn open_cursor(&mut self) -> StreamResult<mongodb::Cursor<LogDocument>> {
        let client = Client::with_uri_str(&self.options.connection_string)
            .await
            .map_err(StreamError::other)?;
        let db = client.database(&self.options.database);
        let collection: Collection<LogDocument> = db.collection(&self.options.collection);

        let filter = build_filter(&self.query);
        let options = build_find_options(&self.query);

        collection
            .find(filter)
            .with_options(options)
            .await
            .map_err(StreamError::other)
    }
}

impl ReadableSource<LogInfo> for MongoDBSource {
    async fn pull(
        &mut self,
        controller: &mut ReadableStreamDefaultController<LogInfo>,
    ) -> StreamResult<()> {
        if !self.initialized {
            let cursor = self.open_cursor().await?;
            self.cursor = Some(cursor);
            self.initialized = true;
        }

        let Some(cursor) = self.cursor.as_mut() else {
            return Ok(());
        };

        match cursor.next().await {
            None => {
                let _ = controller.close();
                self.cursor = None;
            }
            Some(Err(e)) => return Err(StreamError::other(e)),
            Some(Ok(doc)) => {
                let mut log_info = document_to_loginfo(doc);
                if !self.query.fields.is_empty() {
                    apply_field_projection(&mut log_info, &self.query.fields);
                }
                let _ = controller.enqueue(log_info);
            }
        }
        Ok(())
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn build_filter(query: &LogQuery) -> Document {
    let mut filter_parts = Vec::new();

    let mut timestamp_filter = Document::new();
    if let Some(from) = query.from {
        timestamp_filter.insert("$gte", from);
    }
    if let Some(until) = query.until {
        timestamp_filter.insert("$lte", until);
    }
    if !timestamp_filter.is_empty() {
        filter_parts.push(doc! { "timestamp": timestamp_filter });
    }

    if !query.levels.is_empty() {
        filter_parts.push(doc! { "level": { "$in": &query.levels } });
    }

    if let Some(search_regex) = &query.search_term {
        filter_parts.push(doc! { "message": { "$regex": search_regex.as_str() } });
    }

    if let Some(ref dsl_filter) = query.filter {
        filter_parts.push(dsl_filter.to_mongodb_filter());
    }

    if filter_parts.is_empty() {
        Document::new()
    } else if filter_parts.len() == 1 {
        filter_parts.into_iter().next().unwrap()
    } else {
        doc! { "$and": filter_parts }
    }
}

fn build_find_options(query: &LogQuery) -> FindOptions {
    let mut options = FindOptions::default();

    if let Some(start) = query.start {
        options.skip = Some(start as u64);
    }
    if let Some(limit) = query.limit {
        options.limit = Some(limit as i64);
    }

    let sort_direction = match query.order {
        Order::Ascending => 1,
        Order::Descending => -1,
    };
    options.sort = Some(doc! { "timestamp": sort_direction });

    if !query.fields.is_empty() {
        let mut projection = Document::new();
        projection.insert("timestamp", 1);
        for field in query.fields.iter() {
            projection.insert(field, 1);
        }
        options.projection = Some(projection);
    }

    options
}

fn apply_field_projection(log_info: &mut LogInfo, fields: &[String]) {
    let normalized: std::collections::HashSet<&String> = fields.iter().collect();
    if !normalized.contains(&"level".to_string()) {
        log_info.level.clear();
    }
    if !normalized.contains(&"message".to_string()) {
        log_info.message.clear();
    }
    log_info.meta.retain(|k, _| normalized.contains(k));
}

fn document_to_loginfo(doc: LogDocument) -> LogInfo {
    let mut meta = doc.meta;
    meta.insert(
        "timestamp".to_string(),
        serde_json::Value::from(doc.timestamp.to_rfc3339()),
    );
    LogInfo::from_parts(doc.level, doc.message, meta)
}

async fn create_indexes(collection: &Collection<LogDocument>) -> Result<(), mongodb::error::Error> {
    let text_index = IndexModel::builder()
        .keys(doc! { "message": "text" })
        .options(IndexOptions::builder().background(Some(true)).build())
        .build();

    let compound_index = IndexModel::builder()
        .keys(doc! { "level": 1, "timestamp": 1 })
        .options(IndexOptions::builder().background(Some(true)).build())
        .build();

    collection
        .create_indexes(vec![text_index, compound_index])
        .await?;

    Ok(())
}

// ── Spawner helper ──────────────────────────────────────────────────────────

/// Returns a `SpawnFn` that schedules tasks onto the *current* tokio runtime.
///
/// Construct your `Logger` with this when registering a `MongoDBTransport`:
///
/// ```ignore
/// let logger = Logger::new_with_spawner(None, winston_mongodb::tokio_spawner());
/// logger.add_transport(MongoDBTransport::builder(uri, "logs", "events").build());
/// ```
///
/// Must be called from inside a tokio runtime — panics otherwise.
pub fn tokio_spawner() -> Arc<dyn Fn(Pin<Box<dyn Future<Output = ()> + Send + 'static>>) + Send + Sync>
{
    let handle = tokio::runtime::Handle::current();
    Arc::new(move |fut| {
        handle.spawn(fut);
    })
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use mongodb::bson::doc;
    use std::env;
    use whatwg_streams::{CountQueuingStrategy, ReadableStream, WritableStream};
    use winston_transport::BoxedReadableSource;

    fn require_uri() -> Option<String> {
        dotenv::dotenv().ok();
        match env::var("MONGODB_URI") {
            Ok(uri) => Some(uri),
            Err(_) => {
                eprintln!("Skipping test: MONGODB_URI not set");
                None
            }
        }
    }

    #[tokio::test]
    async fn writes_through_writable_stream() {
        let Some(uri) = require_uri() else { return };

        let options = MongoDBOptions {
            connection_string: uri.clone(),
            database: "winston_mongodb_test_db".to_string(),
            collection: "logs".to_string(),
        };

        let transport = MongoDBTransport::new(options.clone());
        let stream = WritableStream::builder(transport)
            .strategy(CountQueuingStrategy::new(8))
            .spawn(|fut| {
                tokio::spawn(fut);
            });

        let (_locked, writer) = stream.get_writer().expect("get_writer");
        writer
            .write(LogInfo::new("info", "writes_through_writable_stream"))
            .await
            .expect("write");
        writer.close().await.expect("close");

        // Verify
        let client = Client::with_uri_str(&options.connection_string).await.unwrap();
        let coll: Collection<LogDocument> = client
            .database(&options.database)
            .collection(&options.collection);
        let filter = doc! { "message": "writes_through_writable_stream" };
        let result = coll.find_one(filter.clone()).await.unwrap();
        assert!(result.is_some());

        // Cleanup
        coll.delete_one(filter).await.unwrap();
    }

    #[tokio::test]
    async fn query_streams_results() {
        let Some(uri) = require_uri() else { return };

        let options = MongoDBOptions {
            connection_string: uri.clone(),
            database: "winston_mongodb_test_db".to_string(),
            collection: "logs_query".to_string(),
        };

        // Cleanup any leftover entries from prior runs
        {
            let client = Client::with_uri_str(&options.connection_string).await.unwrap();
            let coll: Collection<LogDocument> = client
                .database(&options.database)
                .collection(&options.collection);
            coll.delete_many(doc! { "message": { "$regex": "^query_streams_results" } })
                .await
                .unwrap();
        }

        // Insert via WritableStream
        let transport = MongoDBTransport::new(options.clone());
        let stream = WritableStream::builder(transport)
            .strategy(CountQueuingStrategy::new(8))
            .spawn(|fut| {
                tokio::spawn(fut);
            });
        let (_locked, writer) = stream.get_writer().expect("get_writer");
        for i in 0..3 {
            writer
                .write(LogInfo::new("info", &format!("query_streams_results {i}")))
                .await
                .expect("write");
        }
        writer.close().await.expect("close");

        // Query via DynQueryHandle → ReadableStream. We just inserted three
        // unique entries above; level filter scopes to "info" (which all
        // three were) and any pre-existing logs in this collection are
        // cleaned out at the start of the test.
        let read_transport = MongoDBTransport::new(options.clone());
        let handle = read_transport
            .query_handle()
            .expect("query_handle returned None");
        let mut q = LogQuery::new();
        q.levels = vec!["info".to_string()];
        let source = handle.query(&q).expect("query returned None");
        let read_stream = ReadableStream::builder(BoxedReadableSource(source))
            .strategy(CountQueuingStrategy::new(8))
            .spawn(|fut| {
                tokio::spawn(fut);
            });

        let (_locked_r, reader) = read_stream.get_reader().expect("get_reader");
        let mut collected = Vec::new();
        while let Some(entry) = reader.read().await.expect("read") {
            collected.push(entry);
        }
        assert_eq!(collected.len(), 3);

        // Cleanup
        let client = Client::with_uri_str(&options.connection_string).await.unwrap();
        let coll: Collection<LogDocument> = client
            .database(&options.database)
            .collection(&options.collection);
        coll.delete_many(doc! { "message": { "$regex": "^query_streams_results" } })
            .await
            .unwrap();
    }
}
