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
    sync::{Arc, Mutex},
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
use winston_transport::{
    DynIngestHandle, DynQueryHandle, DynReadableSource, LogQuery, Order, Transport,
};

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

    fn ingest_handle(&self) -> Option<Box<dyn DynIngestHandle>> {
        Some(Box::new(MongoDBIngestHandle {
            options: self.options.clone(),
        }))
    }
}

/// Out-of-band ingest target. Each call opens a fresh client + collection and
/// runs `insert_many` for the batch — independent of the live transport's
/// client. Suitable for low/medium-frequency proxy flows; for high-frequency
/// proxying you'd want to cache the client across calls.
pub struct MongoDBIngestHandle {
    options: MongoDBOptions,
}

impl DynIngestHandle for MongoDBIngestHandle {
    fn ingest<'s>(
        &'s self,
        logs: Vec<LogInfo>,
    ) -> Pin<Box<dyn Future<Output = StreamResult<()>> + Send + 's>> {
        Box::pin(async move {
            if logs.is_empty() {
                return Ok(());
            }
            let client = Client::with_uri_str(&self.options.connection_string)
                .await
                .map_err(StreamError::other)?;
            let collection: Collection<LogDocument> = client
                .database(&self.options.database)
                .collection(&self.options.collection);

            let docs: Vec<LogDocument> = logs
                .into_iter()
                .map(|info| LogDocument {
                    timestamp: Utc::now(),
                    level: info.level,
                    message: info.message,
                    meta: info.meta,
                })
                .collect();

            collection
                .insert_many(docs)
                .await
                .map_err(StreamError::other)?;
            Ok(())
        })
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

impl MongoDBQueryHandle {
    /// Destructive query: like [`DynQueryHandle::query`], but records the
    /// `_id` of every document emitted so they can be deleted afterward.
    ///
    /// Returns the source plus a [`MongoDBConsumeToken`]. Drain the source
    /// fully, then call [`MongoDBConsumeToken::delete_consumed`] to remove
    /// exactly the documents that were read — documents inserted after the
    /// cursor opened, or skipped by `start`/`limit`, are untouched. This is
    /// the precise way to "move" logs out of MongoDB into another transport.
    ///
    /// If you delete before the source is fully drained, only the documents
    /// emitted so far are removed.
    pub fn query_consuming(
        &self,
        options: &LogQuery,
    ) -> (Box<dyn DynReadableSource>, MongoDBConsumeToken) {
        let consumed: Arc<Mutex<Vec<bson::oid::ObjectId>>> =
            Arc::new(Mutex::new(Vec::new()));
        let source = MongoDBConsumingSource {
            options: self.options.clone(),
            query: options.clone(),
            cursor: None,
            initialized: false,
            consumed: Arc::clone(&consumed),
        };
        let token = MongoDBConsumeToken {
            options: self.options.clone(),
            consumed,
        };
        (Box::new(source), token)
    }
}

/// Companion to [`MongoDBQueryHandle::query_consuming`]. Holds the `_id`s of
/// every document the paired source emitted; [`Self::delete_consumed`]
/// removes exactly those documents.
pub struct MongoDBConsumeToken {
    options: MongoDBOptions,
    consumed: Arc<Mutex<Vec<bson::oid::ObjectId>>>,
}

impl MongoDBConsumeToken {
    /// Delete the documents recorded as consumed so far. Returns the count
    /// actually deleted. Call once, after the source has been drained.
    ///
    /// Safe to call even if nothing was consumed (returns `Ok(0)` without
    /// hitting the server).
    pub async fn delete_consumed(&self) -> StreamResult<u64> {
        let ids: Vec<bson::oid::ObjectId> = {
            let guard = self.consumed.lock().unwrap();
            guard.clone()
        };
        if ids.is_empty() {
            return Ok(0);
        }
        let client = Client::with_uri_str(&self.options.connection_string)
            .await
            .map_err(StreamError::other)?;
        let collection: Collection<LogDocument> = client
            .database(&self.options.database)
            .collection(&self.options.collection);
        let result = collection
            .delete_many(doc! { "_id": { "$in": ids } })
            .await
            .map_err(StreamError::other)?;
        Ok(result.deleted_count)
    }
}

/// Like [`MongoDBSource`] but reads raw BSON `Document`s so it can capture
/// each `_id` into the shared list before converting to `LogInfo`.
struct MongoDBConsumingSource {
    options: MongoDBOptions,
    query: LogQuery,
    cursor: Option<mongodb::Cursor<Document>>,
    initialized: bool,
    consumed: Arc<Mutex<Vec<bson::oid::ObjectId>>>,
}

impl ReadableSource<LogInfo> for MongoDBConsumingSource {
    async fn pull(
        &mut self,
        controller: &mut ReadableStreamDefaultController<LogInfo>,
    ) -> StreamResult<()> {
        if !self.initialized {
            let client = Client::with_uri_str(&self.options.connection_string)
                .await
                .map_err(StreamError::other)?;
            let collection: Collection<Document> = client
                .database(&self.options.database)
                .collection(&self.options.collection);
            let filter = build_filter(&self.query);
            let options = build_find_options(&self.query);
            let cursor = collection
                .find(filter)
                .with_options(options)
                .await
                .map_err(StreamError::other)?;
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
            Some(Ok(mut raw)) => {
                // Capture `_id`, then strip it so the flattened `meta` in
                // `LogDocument` doesn't pick it up.
                if let Some(bson::Bson::ObjectId(id)) = raw.remove("_id") {
                    self.consumed.lock().unwrap().push(id);
                }
                let log_doc: LogDocument =
                    bson::from_document(raw).map_err(StreamError::other)?;
                let mut log_info = document_to_loginfo(log_doc);
                if !self.query.fields.is_empty() {
                    apply_field_projection(&mut log_info, &self.query.fields);
                }
                let _ = controller.enqueue(log_info);
            }
        }
        Ok(())
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

    /// Verifies `ingest_handle`: extract a handle, ingest a batch, confirm
    /// the entries land in the collection. The legacy `Proxy::ingest`
    /// equivalent: out-of-band batch acceptance from another transport.
    #[tokio::test]
    async fn ingest_handle_inserts_batch() {
        let Some(uri) = require_uri() else { return };

        let options = MongoDBOptions {
            connection_string: uri.clone(),
            database: "winston_mongodb_test_db".to_string(),
            collection: "logs_ingest".to_string(),
        };

        // Cleanup any leftover entries from prior runs.
        let client = Client::with_uri_str(&options.connection_string).await.unwrap();
        let coll: Collection<LogDocument> = client
            .database(&options.database)
            .collection(&options.collection);
        coll.delete_many(doc! { "message": { "$regex": "^ingest_handle_inserts" } })
            .await
            .unwrap();

        let transport = MongoDBTransport::new(options.clone());
        let handle = transport.ingest_handle().expect("ingest_handle is Some");

        handle
            .ingest(vec![
                LogInfo::new("info", "ingest_handle_inserts a"),
                LogInfo::new("warn", "ingest_handle_inserts b"),
            ])
            .await
            .expect("ingest");

        // Verify both ended up in the collection.
        use futures::TryStreamExt;
        let mut cursor = coll
            .find(doc! { "message": { "$regex": "^ingest_handle_inserts" } })
            .await
            .unwrap();
        let mut found = Vec::new();
        while let Some(doc) = cursor.try_next().await.unwrap() {
            found.push(doc.message);
        }
        found.sort();
        assert_eq!(
            found,
            vec![
                "ingest_handle_inserts a".to_string(),
                "ingest_handle_inserts b".to_string(),
            ]
        );

        // Cleanup
        coll.delete_many(doc! { "message": { "$regex": "^ingest_handle_inserts" } })
            .await
            .unwrap();
    }

    /// Verifies `query_consuming` + `delete_consumed`: insert N docs, drain
    /// them through the consuming source, delete exactly those, confirm the
    /// collection no longer holds them — and that a doc inserted *after* the
    /// cursor opened is NOT deleted.
    #[tokio::test]
    async fn query_consuming_then_delete_removes_exactly_consumed() {
        let Some(uri) = require_uri() else { return };

        let options = MongoDBOptions {
            connection_string: uri.clone(),
            database: "winston_mongodb_test_db".to_string(),
            collection: "logs_consume".to_string(),
        };

        let client = Client::with_uri_str(&options.connection_string).await.unwrap();
        let coll: Collection<LogDocument> = client
            .database(&options.database)
            .collection(&options.collection);
        coll.delete_many(doc! { "message": { "$regex": "^consume_test" } })
            .await
            .unwrap();

        // Insert three docs to consume.
        let ingest = MongoDBIngestHandle {
            options: options.clone(),
        };
        ingest
            .ingest(vec![
                LogInfo::new("info", "consume_test a"),
                LogInfo::new("info", "consume_test b"),
                LogInfo::new("info", "consume_test c"),
            ])
            .await
            .expect("seed ingest");

        // Open the consuming source over those three.
        let handle = MongoDBQueryHandle {
            options: options.clone(),
        };
        let mut q = LogQuery::new();
        q.levels = vec!["info".to_string()];
        // Note: this query matches *all* info docs; we rely on the regex
        // cleanup above to keep the collection scoped to this test's data.
        let (source, token) = handle.query_consuming(&q);

        // Drain the source.
        let read_stream = ReadableStream::builder(BoxedReadableSource(source))
            .strategy(CountQueuingStrategy::new(8))
            .spawn(|fut| {
                tokio::spawn(fut);
            });
        let (_locked, reader) = read_stream.get_reader().expect("get_reader");
        let mut drained = Vec::new();
        while let Some(entry) = reader.read().await.expect("read") {
            drained.push(entry);
        }
        assert_eq!(drained.len(), 3);

        // Insert a fourth doc AFTER the cursor was drained — it must survive
        // the delete because its _id wasn't recorded.
        ingest
            .ingest(vec![LogInfo::new("info", "consume_test d_after")])
            .await
            .expect("post-drain ingest");

        // Delete exactly the consumed docs.
        let deleted = token.delete_consumed().await.expect("delete_consumed");
        assert_eq!(deleted, 3, "should delete exactly the 3 consumed docs");

        // The fourth doc should remain.
        use futures::TryStreamExt;
        let mut cursor = coll
            .find(doc! { "message": { "$regex": "^consume_test" } })
            .await
            .unwrap();
        let mut remaining = Vec::new();
        while let Some(d) = cursor.try_next().await.unwrap() {
            remaining.push(d.message);
        }
        assert_eq!(remaining, vec!["consume_test d_after".to_string()]);

        // Cleanup
        coll.delete_many(doc! { "message": { "$regex": "^consume_test" } })
            .await
            .unwrap();
    }
}
