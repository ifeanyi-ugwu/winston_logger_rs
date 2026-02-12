mod to_mongodb_filter;

use chrono::{DateTime, Utc};
use futures::StreamExt;
use logform::LogInfo;
use mongodb::{
    bson::{self, doc, Document},
    options::{FindOptions, IndexOptions},
    Client, Collection, IndexModel,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    thread,
};
use to_mongodb_filter::ToMongoDbFilter;
use tokio::runtime::Builder as TokioBuilder;
use winston_transport::{LogQuery, Order, Transport};

#[derive(Debug, Serialize, Deserialize)]
struct LogDocument {
    #[serde(with = "bson::serde_helpers::chrono_datetime_as_bson_datetime")]
    timestamp: DateTime<Utc>,
    level: String,
    message: String,
    #[serde(flatten)]
    meta: HashMap<String, serde_json::Value>,
}

pub struct MongoDBTransport {
    sender: mpsc::Sender<MongoDBThreadMessage>,
    #[cfg(test)]
    options: MongoDBOptions,
    exit_signal: Arc<AtomicBool>,
}

enum MongoDBThreadMessage {
    Log(LogDocument),
    LogBatch(Vec<LogDocument>),
    Query(LogQuery, mpsc::Sender<Result<Vec<LogInfo>, String>>),
    Shutdown,
}

#[derive(Clone)]
pub struct MongoDBOptions {
    pub connection_string: String,
    pub database: String,
    pub collection: String,
}

/// The task that needs to be spawned/driven to completion
pub type MongoDBTask = Pin<Box<dyn Future<Output = ()> + Send>>;

/// Default spawn function using std::thread and tokio runtime
///
/// This is a convenience function that can be passed to `builder().spawn()`
/// to use the default spawning strategy (std::thread + tokio::block_on).
///
/// # Example
///
/// ```ignore
/// use winston_mongodb::spawn_with_tokio_thread;
///
/// let transport = MongoDBTransport::builder(uri, "db", "logs")
///     .spawn(spawn_with_tokio_thread);
/// ```
pub fn spawn_with_tokio_thread(task: MongoDBTask) {
    thread::spawn(move || {
        let rt = TokioBuilder::new_current_thread()
            .enable_time()
            .enable_io()
            .build()
            .unwrap();

        rt.block_on(task);
    });
}

/// Builder for MongoDBTransport
pub struct MongoDBTransportBuilder {
    connection_string: String,
    database: String,
    collection: String,
}

impl MongoDBTransportBuilder {
    /// Create a new builder with required parameters
    pub fn new(
        connection_string: impl Into<String>,
        database: impl Into<String>,
        collection: impl Into<String>,
    ) -> Self {
        Self {
            connection_string: connection_string.into(),
            database: database.into(),
            collection: collection.into(),
        }
    }

    /// Build the transport and return both the transport handle and the task
    ///
    /// The user is responsible for spawning/driving the task to completion.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// // With tokio::spawn
    /// let (transport, task) = MongoDBTransport::builder(uri, "db", "logs").build();
    /// tokio::spawn(task);
    ///
    /// // With async_std
    /// let (transport, task) = MongoDBTransport::builder(uri, "db", "logs").build();
    /// async_std::task::spawn(task);
    ///
    /// // With thread + block_on
    /// let (transport, task) = MongoDBTransport::builder(uri, "db", "logs").build();
    /// std::thread::spawn(|| {
    ///     tokio::runtime::Runtime::new().unwrap().block_on(task);
    /// });
    /// ```
    pub fn build(self) -> (MongoDBTransport, MongoDBTask) {
        let options = MongoDBOptions {
            connection_string: self.connection_string,
            database: self.database,
            collection: self.collection,
        };

        MongoDBTransport::new_inner(options)
    }

    /// Spawn the transport with a custom spawn function and return only the transport
    ///
    /// This is a convenience method that automatically spawns the task.
    /// The spawn function receives the task and is responsible for executing it.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// // With tokio::spawn
    /// let transport = MongoDBTransport::builder(uri, "db", "logs")
    ///     .spawn(|task| { tokio::spawn(task); });
    ///
    /// // With async_std
    /// let transport = MongoDBTransport::builder(uri, "db", "logs")
    ///     .spawn(|task| { async_std::task::spawn(task); });
    ///
    /// // With thread + block_on
    /// let transport = MongoDBTransport::builder(uri, "db", "logs")
    ///     .spawn(|task| {
    ///         std::thread::spawn(|| {
    ///             tokio::runtime::Runtime::new().unwrap().block_on(task);
    ///         });
    ///     });
    /// ```
    pub fn spawn<F>(self, spawn_fn: F) -> MongoDBTransport
    where
        F: FnOnce(MongoDBTask),
    {
        let (transport, task) = self.build();
        spawn_fn(task);
        transport
    }
}

impl MongoDBTransport {
    /// Create a new MongoDBTransport using the builder pattern
    pub fn builder(
        connection_string: impl Into<String>,
        database: impl Into<String>,
        collection: impl Into<String>,
    ) -> MongoDBTransportBuilder {
        MongoDBTransportBuilder::new(connection_string, database, collection)
    }

    /// Create a new MongoDBTransport with default spawn function (legacy method)
    ///
    /// This automatically spawns the task in a std::thread with tokio runtime.
    /// For more control, use `builder().build()` or `builder().spawn()` instead.
    ///
    /// # Example
    ///
    /// ```ignore
    /// // Legacy way
    /// let transport = MongoDBTransport::new(options)?;
    ///
    /// // Preferred way (equivalent)
    /// let transport = MongoDBTransport::builder(uri, "db", "logs")
    ///     .spawn(spawn_with_tokio_thread);
    /// ```
    pub fn new(options: MongoDBOptions) -> Result<Self, mongodb::error::Error> {
        let (transport, task) = Self::new_inner(options);
        spawn_with_tokio_thread(task);
        Ok(transport)
    }

    /// Internal: Create the transport and task
    ///
    /// Returns (transport_handle, background_task)
    /// The user must spawn/drive the task to completion.
    fn new_inner(options: MongoDBOptions) -> (Self, MongoDBTask) {
        let (sender, receiver) = mpsc::channel();
        let exit_signal = Arc::new(AtomicBool::new(false));
        let exit_signal_clone = exit_signal.clone();
        let options_for_task = options.clone();

        let task = Box::pin(async move {
            let client = Client::with_uri_str(&options_for_task.connection_string)
                .await
                .unwrap();
            let db = client.database(&options_for_task.database);
            let collection = db.collection::<LogDocument>(&options_for_task.collection);

            create_indexes(&collection).await.unwrap();

            while !exit_signal_clone.load(Ordering::Relaxed) {
                match receiver.recv_timeout(std::time::Duration::from_millis(100)) {
                    Ok(MongoDBThreadMessage::Log(log_doc)) => {
                        if let Err(e) = collection.insert_one(log_doc).await {
                            eprintln!("Failed to write to MongoDB: {}", e);
                        }
                    }
                    Ok(MongoDBThreadMessage::LogBatch(log_docs)) => {
                        if !log_docs.is_empty() {
                            if let Err(e) = collection.insert_many(log_docs).await {
                                eprintln!("Failed to write batch to MongoDB: {}", e);
                            }
                        }
                    }
                    Ok(MongoDBThreadMessage::Query(query, response_tx)) => {
                        let result = Self::execute_query(&collection, &query).await;
                        let _ = response_tx.send(result);
                    }
                    Ok(MongoDBThreadMessage::Shutdown) => break,
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        });

        let transport = Self {
            sender,
            #[cfg(test)]
            options,
            exit_signal,
        };

        (transport, task)
    }

    /// Shutdown the transport
    ///
    /// Signals the background task to stop. The task will complete its current operation
    /// and then exit.
    pub fn shutdown(&self) {
        let _ = self.sender.send(MongoDBThreadMessage::Shutdown);
        self.exit_signal.store(true, Ordering::Relaxed);
    }

    #[cfg(test)]
    async fn get_collection(&self) -> Collection<LogDocument> {
        let client = Client::with_uri_str(&self.options.connection_string)
            .await
            .unwrap();
        let db = client.database(&self.options.database);
        db.collection(&self.options.collection)
    }

    async fn execute_query(
        collection: &Collection<LogDocument>,
        query: &LogQuery,
    ) -> Result<Vec<LogInfo>, String> {
        let mut filter_parts = Vec::new();

        // Add timestamp range filters
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

        // Add level filter
        if !query.levels.is_empty() {
            filter_parts.push(doc! { "level": { "$in": &query.levels } });
        }

        // Add search term filter
        if let Some(search_regex) = &query.search_term {
            filter_parts.push(doc! {
                "message": {
                    "$regex": search_regex.as_str()
                }
            });
        }

        // Add DSL filter if present
        if let Some(ref dsl_filter) = query.filter {
            filter_parts.push(dsl_filter.to_mongodb_filter());
        }

        // Combine all filters with $and if there are multiple conditions
        let filter = if filter_parts.is_empty() {
            Document::new()
        } else if filter_parts.len() == 1 {
            filter_parts.into_iter().next().unwrap()
        } else {
            doc! { "$and": filter_parts }
        };

        // Configure options (sort, skip, limit)
        let mut options = FindOptions::default();

        // Apply start (skip) and limit
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

        // Apply MongoDB Projection (Query Optimization)
        if !query.fields.is_empty() {
            let mut projection = Document::new();
            projection.insert("timestamp", 1);

            for field in query.fields.iter() {
                projection.insert(field, 1);
            }

            options.projection = Some(projection);
        }

        let mut cursor = collection
            .find(filter)
            .with_options(options)
            .await
            .map_err(|e| format!("Failed to execute MongoDB query: {}", e))?;

        let mut results = Vec::new();
        let normalized_fields: std::collections::HashSet<&String> = query.fields.iter().collect();

        while let Some(result) = cursor.next().await {
            match result {
                Ok(doc) => {
                    let mut log_info = document_to_loginfo(doc);

                    // Apply user-requested-field Projection (Response Filtering)
                    if !query.fields.is_empty() {
                        if !normalized_fields.contains(&"level".to_string()) {
                            log_info.level.clear();
                        }
                        if !normalized_fields.contains(&"message".to_string()) {
                            log_info.message.clear();
                        }
                        log_info.meta.retain(|k, _| normalized_fields.contains(k));
                    }

                    results.push(log_info);
                }
                Err(e) => return Err(format!("Error reading MongoDB document: {}", e)),
            }
        }

        Ok(results)
    }
}

fn document_to_loginfo(doc: LogDocument) -> LogInfo {
    let mut meta = doc.meta;
    meta.insert(
        "timestamp".to_string(),
        serde_json::Value::from(doc.timestamp.to_rfc3339()),
    );

    LogInfo {
        level: doc.level,
        message: doc.message,
        meta,
    }
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

impl Transport<LogInfo> for MongoDBTransport {
    fn log(&self, info: LogInfo) {
        let doc = LogDocument {
            timestamp: Utc::now(),
            level: info.level,
            message: info.message,
            meta: info.meta,
        };

        if let Err(e) = self.sender.send(MongoDBThreadMessage::Log(doc)) {
            eprintln!("Failed to send log to the logging thread: {}", e);
        }
    }

    fn log_batch(&self, logs: Vec<LogInfo>) {
        let docs: Vec<LogDocument> = logs
            .into_iter()
            .map(|info| LogDocument {
                timestamp: Utc::now(),
                level: info.level,
                message: info.message,
                meta: info.meta,
            })
            .collect();

        if let Err(e) = self.sender.send(MongoDBThreadMessage::LogBatch(docs)) {
            eprintln!("Failed to send log batch to the logging thread: {}", e);
        }
    }

    fn query(&self, query: &LogQuery) -> Result<Vec<LogInfo>, String> {
        let (response_tx, response_rx) = mpsc::channel();

        self.sender
            .send(MongoDBThreadMessage::Query(query.clone(), response_tx))
            .map_err(|e| format!("Failed to send query: {}", e))?;

        response_rx
            .recv()
            .map_err(|e| format!("Failed to receive query response: {}", e))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mongodb::{bson::doc, options::ClientOptions};
    use std::env;

    #[tokio::test]
    async fn test_logging_persists_to_mongodb() {
        dotenv::dotenv().ok();

        let connection_string = match env::var("MONGODB_URI") {
            Ok(uri) => uri,
            Err(_) => {
                eprintln!("Skipping test: MONGODB_URI not set");
                return;
            }
        };

        let options = MongoDBOptions {
            connection_string,
            database: "winston_mongodb_test_db".to_string(),
            collection: "logs".to_string(),
        };

        let transport = MongoDBTransport::new(options.clone()).unwrap();

        let log_info = LogInfo {
            level: "info".to_string(),
            message: "Test log message".to_string(),
            meta: HashMap::new(),
        };

        transport.log(log_info);

        // Allow some time for the log to be inserted
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        // Verify if the log exists in the database
        let client = Client::with_options(
            ClientOptions::parse(&options.connection_string)
                .await
                .unwrap(),
        )
        .unwrap();
        let db = client.database(&options.database);
        let collection = db.collection::<LogDocument>(&options.collection);

        let filter = doc! { "message": "Test log message" };
        let result = collection.find_one(filter.clone()).await.unwrap();

        assert!(result.is_some(), "Log entry was not found in MongoDB");

        // Cleanup: Delete the test log
        collection.delete_one(filter).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_query_logs_from_mongodb() {
        dotenv::dotenv().ok();

        let connection_string = match env::var("MONGODB_URI") {
            Ok(uri) => uri,
            Err(_) => {
                eprintln!("Skipping test: MONGODB_URI not set");
                return;
            }
        };

        let options = MongoDBOptions {
            connection_string,
            database: "winston_mongodb_test_db".to_string(),
            collection: "logs".to_string(),
        };

        let transport = MongoDBTransport::new(options.clone()).unwrap();

        // Insert multiple logs with different levels, timestamps, and messages
        let log_entries = vec![
            LogInfo {
                level: "info".to_string(),
                message: "Info log 1".to_string(),
                meta: HashMap::new(),
            },
            LogInfo {
                level: "warn".to_string(),
                message: "Warning log".to_string(),
                meta: HashMap::new(),
            },
            LogInfo {
                level: "error".to_string(),
                message: "Error log 1".to_string(),
                meta: HashMap::new(),
            },
            LogInfo {
                level: "info".to_string(),
                message: "Info log 2".to_string(),
                meta: HashMap::new(),
            },
        ];

        // Log the entries
        for log_info in log_entries {
            transport.log(log_info);
        }

        // Allow some time for the logs to be inserted and the index created(+2 secs)
        tokio::time::sleep(std::time::Duration::from_secs(2 + 2)).await;

        let query = LogQuery::new()
            .from("a day ago")
            .until("now")
            .levels(vec!["info", "warn"])
            .search_term("log 1")
            .start(0)
            .limit(5)
            .order("desc")
            .fields(vec!["level", "message"]);

        // Perform the query
        let results = transport.query(&query).unwrap();

        // Assert that the query results match expected conditions
        assert_eq!(results.len(), 1, "Query should return 1 result");

        let levels: Vec<String> = results.iter().map(|log| log.level.clone()).collect();
        let messages: Vec<String> = results.iter().map(|log| log.message.clone()).collect();

        // Assert that the logs contain the correct levels and messages based on the query
        assert!(levels.contains(&"info".to_string()) || levels.contains(&"warn".to_string()));
        assert!(
            messages.contains(&"Info log 1".to_string())
                || messages.contains(&"Error log 1".to_string())
        );

        // Assert that only the fields specified in the query are returned
        for log in results {
            assert!(
                log.meta.is_empty(),
                "Meta data should not be included in query results"
            );
        }

        // **Cleanup: Delete all test logs added in this test**
        let collection = transport.get_collection().await;
        let cleanup_filter = doc! { "message": { "$in": ["Info log 1", "Warning log", "Error log 1", "Info log 2"] } };
        let delete_result = collection.delete_many(cleanup_filter).await.unwrap();
        println!("Deleted {} test logs", delete_result.deleted_count);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_dsl_query_filter_with_mongodb() {
        use winston_transport::query_dsl::dlc::alpha::a::prelude::*;
        use winston_transport::{and, field_logic as fl, field_query as fq};

        dotenv::dotenv().ok();

        let connection_string = match env::var("MONGODB_URI") {
            Ok(uri) => uri,
            Err(_) => {
                eprintln!("Skipping test: MONGODB_URI not set");
                return;
            }
        };

        let options = MongoDBOptions {
            connection_string,
            database: "winston_mongodb_test_db".to_string(),
            collection: "logs_dsl_test".to_string(),
        };

        let transport = MongoDBTransport::new(options.clone()).unwrap();

        // Cleanup any existing test data first
        let collection = transport.get_collection().await;
        let cleanup_filter = doc! {
            "message": {
                "$in": [
                    "User login successful",
                    "User profile updated",
                    "Failed login attempt",
                    "Password reset",
                    "Database connection failed"
                ]
            }
        };
        collection
            .delete_many(cleanup_filter.clone())
            .await
            .unwrap();

        // Insert test logs with metadata
        let test_logs = vec![
            LogInfo::new("info", "User login successful")
                .with_meta("user_id", 101)
                .with_meta("user_age", 25)
                .with_meta("user_status", "active")
                .with_meta("department", "engineering"),
            LogInfo::new("info", "User profile updated")
                .with_meta("user_id", 102)
                .with_meta("user_age", 30)
                .with_meta("user_status", "active")
                .with_meta("department", "marketing"),
            LogInfo::new("warn", "Failed login attempt")
                .with_meta("user_id", 103)
                .with_meta("user_age", 45)
                .with_meta("user_status", "suspended")
                .with_meta("department", "engineering"),
            LogInfo::new("info", "Password reset")
                .with_meta("user_id", 104)
                .with_meta("user_age", 22)
                .with_meta("user_status", "active")
                .with_meta("department", "sales"),
            LogInfo::new("error", "Database connection failed")
                .with_meta("user_id", 105)
                .with_meta("user_age", 35)
                .with_meta("user_status", "active")
                .with_meta("department", "engineering"),
        ];

        // Log all entries
        for log in test_logs {
            transport.log(log);
        }

        // Wait for logs to be inserted
        tokio::time::sleep(std::time::Duration::from_secs(4)).await;

        // Test 1: Simple equality filter - users in engineering department
        // Meta fields are flattened in MongoDB documents, so field paths do not include "meta." prefix
        let query1 = LogQuery::new()
            .from("a day ago")
            .until("now")
            .limit(100) // Override default limit
            .filter(fq!("department", eq("engineering")));

        let results1 = transport.query(&query1).unwrap();
        assert_eq!(
            results1.len(),
            3,
            "Should find 3 logs from engineering department"
        );
        for log in &results1 {
            assert_eq!(
                log.meta.get("department").unwrap().as_str().unwrap(),
                "engineering"
            );
        }

        // Test 2: Age range filter - users between 18 and 40
        let query2 = LogQuery::new()
            .from("a day ago")
            .until("now")
            .filter(fq!("user_age", fl!(and, gt(18), lt(40))));

        let results2 = transport.query(&query2).unwrap();
        assert!(
            results2.len() >= 3,
            "Should find at least 3 users aged between 18 and 40"
        );
        for log in &results2 {
            let age = log.meta.get("user_age").unwrap().as_i64().unwrap();
            assert!(age > 18 && age < 40, "Age should be between 18 and 40");
        }

        // Test 3: Complex AND filter - active users in engineering with age > 20
        let query3 = LogQuery::new()
            .from("a day ago")
            .until("now")
            .levels(vec!["info", "warn", "error"])
            .filter(and!(
                fq!("user_status", eq("active")),
                fq!("department", eq("engineering")),
                fq!("user_age", gt(20))
            ));

        let results3 = transport.query(&query3).unwrap();
        assert_eq!(
            results3.len(),
            2,
            "Should find 2 active engineering users over 20"
        );
        for log in &results3 {
            assert_eq!(
                log.meta.get("user_status").unwrap().as_str().unwrap(),
                "active"
            );
            assert_eq!(
                log.meta.get("department").unwrap().as_str().unwrap(),
                "engineering"
            );
            let age = log.meta.get("user_age").unwrap().as_i64().unwrap();
            assert!(age > 20);
        }

        // Test 4: OR filter - users from engineering OR marketing
        let query4 = LogQuery::new()
            .from("a day ago")
            .until("now")
            .filter(winston_transport::or!(
                fq!("department", eq("engineering")),
                fq!("department", eq("marketing"))
            ));

        let results4 = transport.query(&query4).unwrap();
        assert_eq!(
            results4.len(),
            4,
            "Should find 4 logs from engineering or marketing"
        );
        for log in &results4 {
            let dept = log.meta.get("department").unwrap().as_str().unwrap();
            assert!(dept == "engineering" || dept == "marketing");
        }

        // Test 5: Combined filter with levels and DSL
        let query5 = LogQuery::new()
            .from("a day ago")
            .until("now")
            .levels(vec!["info"]) // Only info level
            .filter(and!(
                fq!("user_status", eq("active")),
                fq!("user_age", fl!(and, gt(20), lt(35)))
            ));

        let results5 = transport.query(&query5).unwrap();
        assert_eq!(
            results5.len(),
            3,
            "Should find 3 active info logs with age 20-35"
        );
        for log in &results5 {
            assert_eq!(log.level, "info");
            assert_eq!(
                log.meta.get("user_status").unwrap().as_str().unwrap(),
                "active"
            );
            let age = log.meta.get("user_age").unwrap().as_i64().unwrap();
            assert!(
                age > 20 && age < 35,
                "Age {} should be between 20 and 35",
                age
            );
        }

        println!("All DSL query filter tests passed!");

        // Cleanup: Delete all test logs
        let collection = transport.get_collection().await;
        let cleanup_filter = doc! {
            "message": {
                "$in": [
                    "User login successful",
                    "User profile updated",
                    "Failed login attempt",
                    "Password reset",
                    "Database connection failed"
                ]
            }
        };
        let delete_result = collection.delete_many(cleanup_filter).await.unwrap();
        println!("Deleted {} test logs", delete_result.deleted_count);
    }
}
