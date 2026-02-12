use crate::{log_query::LogQuery, Transport};
use std::{
    marker::PhantomData,
    sync::mpsc::{self, Sender},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

/// Configuration for batch behavior
#[derive(Debug, Clone)]
pub struct BatchConfig {
    /// Maximum number of logs to batch before flushing
    pub max_batch_size: usize,
    /// Maximum time to wait before flushing a partial batch
    pub max_batch_time: Duration,
    /// Whether to flush immediately on Drop
    pub flush_on_drop: bool,
}

impl Default for BatchConfig {
    fn default() -> Self {
        Self {
            max_batch_size: 100,
            max_batch_time: Duration::from_millis(500),
            flush_on_drop: true,
        }
    }
}

/// Internal message types for the batch thread
#[derive(Debug)]
enum BatchMessage<L> {
    Log(L),
    Flush(Sender<Result<(), String>>),
    Query(Box<LogQuery>, Sender<Result<Vec<L>, String>>),
    Shutdown,
}

/// A transport wrapper that batches log messages before sending them to the underlying transport
/// Generic over any log type `L` and transport type `T`.
pub struct BatchedTransport<T, L>
where
    T: Transport<L> + Send + 'static,
    L: Send + 'static,
{
    sender: Sender<BatchMessage<L>>,
    thread_handle: Option<JoinHandle<()>>,
    config: BatchConfig,
    _phantom: PhantomData<(T, L)>,
}

impl<T, L> BatchedTransport<T, L>
where
    T: Transport<L> + Send + 'static,
    L: Send + 'static,
{
    /// Creates a new BatchedTransport with default configuration
    pub fn new(transport: T) -> Self {
        Self::with_config(transport, BatchConfig::default())
    }

    /// Creates a new BatchedTransport with custom configuration
    pub fn with_config(transport: T, config: BatchConfig) -> Self {
        let (sender, receiver) = mpsc::channel();
        let batch_config = config.clone();

        let thread_handle = thread::spawn(move || {
            Self::run_batch_thread(transport, receiver, batch_config);
        });

        Self {
            sender,
            thread_handle: Some(thread_handle),
            config,
            _phantom: PhantomData,
        }
    }

    /// Creates a BatchedTransport with a custom thread name
    pub fn with_thread_name(transport: T, config: BatchConfig, thread_name: String) -> Self {
        let (sender, receiver) = mpsc::channel();
        let batch_config = config.clone();

        let thread_handle = thread::Builder::new()
            .name(thread_name)
            .spawn(move || {
                Self::run_batch_thread(transport, receiver, batch_config);
            })
            .expect("Failed to spawn batch transport thread");

        Self {
            sender,
            thread_handle: Some(thread_handle),
            config,
            _phantom: PhantomData,
        }
    }

    fn run_batch_thread(
        transport: T,
        receiver: mpsc::Receiver<BatchMessage<L>>,
        config: BatchConfig,
    ) {
        let mut batch = Vec::new();
        let mut last_flush = Instant::now();

        let flush_batch = |batch: &mut Vec<L>| -> Result<(), String> {
            if !batch.is_empty() {
                transport.log_batch(std::mem::take(batch));
                transport.flush() // propagate flush error
            } else {
                Ok(())
            }
        };

        loop {
            let time_since_last_flush = last_flush.elapsed();
            let timeout = if batch.is_empty() {
                None
            } else if time_since_last_flush >= config.max_batch_time {
                Some(Duration::from_millis(0))
            } else {
                Some(config.max_batch_time - time_since_last_flush)
            };

            let message_result = if let Some(timeout) = timeout {
                receiver.recv_timeout(timeout)
            } else {
                receiver
                    .recv()
                    .map_err(|_| mpsc::RecvTimeoutError::Disconnected)
            };

            match message_result {
                Ok(BatchMessage::Log(info)) => {
                    batch.push(info);
                    if batch.len() >= config.max_batch_size {
                        let _ = flush_batch(&mut batch);
                        last_flush = Instant::now();
                    }
                }
                Ok(BatchMessage::Flush(response_sender)) => {
                    let result = flush_batch(&mut batch);
                    last_flush = Instant::now();
                    let _ = response_sender.send(result);
                }
                Ok(BatchMessage::Query(query, response_sender)) => {
                    let _ = flush_batch(&mut batch);
                    last_flush = Instant::now();
                    let result = transport.query(&query);
                    let _ = response_sender.send(result);
                }
                Ok(BatchMessage::Shutdown) => {
                    let _ = flush_batch(&mut batch);
                    break;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if !batch.is_empty() && last_flush.elapsed() >= config.max_batch_time {
                        let _ = flush_batch(&mut batch);
                        last_flush = Instant::now();
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    let _ = flush_batch(&mut batch);
                    break;
                }
            }
        }
    }

    /// Gracefully shuts down the batching thread
    pub fn shutdown(mut self) -> Result<(), String> {
        if let Some(handle) = self.thread_handle.take() {
            self.sender
                .send(BatchMessage::Shutdown)
                .map_err(|_| "Failed to send shutdown signal")?;

            handle.join().map_err(|_| "Failed to join batch thread")?;
        }
        Ok(())
    }

    /// Gets the current batch configuration
    pub fn config(&self) -> &BatchConfig {
        &self.config
    }
}

impl<T, L> Transport<L> for BatchedTransport<T, L>
where
    T: Transport<L> + Send + 'static,
    L: Send + 'static,
{
    fn log(&self, info: L) {
        let _ = self.sender.send(BatchMessage::Log(info));
    }

    fn flush(&self) -> Result<(), String> {
        let (response_sender, response_receiver) = mpsc::channel();

        self.sender
            .send(BatchMessage::Flush(response_sender))
            .map_err(|_| "Failed to send flush message to batch thread")?;

        response_receiver
            .recv()
            .map_err(|_| "Failed to receive flush response from batch thread")?
    }

    fn query(&self, options: &LogQuery) -> Result<Vec<L>, String> {
        let (response_sender, response_receiver) = mpsc::channel();

        self.sender
            .send(BatchMessage::Query(
                Box::new(options.clone()),
                response_sender,
            ))
            .map_err(|_| "Failed to send query message to batch thread")?;

        response_receiver
            .recv()
            .map_err(|_| "Failed to receive query response from batch thread")?
    }
}

impl<T, L> Drop for BatchedTransport<T, L>
where
    T: Transport<L> + Send + 'static,
    L: Send + 'static,
{
    fn drop(&mut self) {
        if self.config.flush_on_drop {
            if let Some(handle) = self.thread_handle.take() {
                let _ = self.sender.send(BatchMessage::Shutdown);
                let _ = handle.join();
            }
        }
    }
}

impl<T, L> Clone for BatchedTransport<T, L>
where
    T: Transport<L> + Send + 'static,
    L: Send + 'static,
{
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            thread_handle: None, // Don't clone thread handle because thread is owned by original
            config: self.config.clone(),
            _phantom: PhantomData,
        }
    }
}

/// Extension trait for easily wrapping any transport with batching behavior
/// Generic over any log type `L`.
pub trait IntoBatchedTransport<L>: Transport<L> + Send + Sized + 'static
where
    L: Send + 'static,
{
    /// Wraps this transport in a BatchedTransport with default configuration
    fn into_batched(self) -> BatchedTransport<Self, L> {
        BatchedTransport::new(self)
    }

    /// Wraps this transport in a BatchedTransport with custom configuration
    fn into_batched_with_config(self, config: BatchConfig) -> BatchedTransport<Self, L> {
        BatchedTransport::with_config(self, config)
    }

    /// Wraps this transport in a BatchedTransport with a custom thread name
    fn into_batched_named(
        self,
        config: BatchConfig,
        thread_name: String,
    ) -> BatchedTransport<Self, L> {
        BatchedTransport::with_thread_name(self, config, thread_name)
    }
}

impl<T, L> IntoBatchedTransport<L> for T
where
    T: Transport<L> + Send + Sized + 'static,
    L: Send + 'static,
{
}

#[cfg(test)]
mod tests {

    use super::*;
    use std::{
        fmt::Display,
        sync::{Arc, Mutex},
        time::Duration,
    };

    // Generic test log type for testing - completely generic!
    #[derive(Clone, Debug, PartialEq)]
    struct TestLog {
        level: String,
        message: String,
    }

    impl TestLog {
        fn new(level: &str, message: &str) -> Self {
            Self {
                level: level.to_string(),
                message: message.to_string(),
            }
        }
    }

    impl Display for TestLog {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "[{}] {}", self.level, self.message)
        }
    }

    // Generic mock transport that works with any log type
    #[derive(Clone)]
    struct MockTransport<L>
    where
        L: Clone + Send + 'static,
    {
        messages: Arc<Mutex<Vec<L>>>,
        log_calls: Arc<Mutex<Vec<Instant>>>,
    }

    impl<L> MockTransport<L>
    where
        L: Clone + Send + 'static,
    {
        fn new() -> Self {
            Self {
                messages: Arc::new(Mutex::new(Vec::new())),
                log_calls: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn get_messages(&self) -> Vec<L> {
            self.messages.lock().unwrap().clone()
        }

        fn get_log_call_count(&self) -> usize {
            self.log_calls.lock().unwrap().len()
        }
    }

    impl<L> Transport<L> for MockTransport<L>
    where
        L: Clone + Send + 'static,
    {
        fn log(&self, info: L) {
            self.messages.lock().unwrap().push(info);
            self.log_calls.lock().unwrap().push(Instant::now());
        }

        fn flush(&self) -> Result<(), String> {
            Ok(())
        }
    }

    #[test]
    fn test_batch_size_trigger() {
        let mock: MockTransport<TestLog> = MockTransport::new();
        let mock_clone = mock.clone();

        let config = BatchConfig {
            max_batch_size: 3,
            max_batch_time: Duration::from_secs(10),
            flush_on_drop: true,
        };

        let batched = mock.into_batched_with_config(config);

        batched.log(TestLog::new("INFO", "Message 1"));
        batched.log(TestLog::new("INFO", "Message 2"));
        batched.log(TestLog::new("INFO", "Message 3"));

        // Allow thread to process batch
        std::thread::sleep(Duration::from_millis(100));

        let messages = mock_clone.get_messages();
        assert_eq!(messages.len(), 3);
        assert_eq!(mock_clone.get_log_call_count(), 3);
    }

    #[test]
    fn test_time_trigger() {
        let mock: MockTransport<TestLog> = MockTransport::new();
        let mock_clone = mock.clone();

        let config = BatchConfig {
            max_batch_size: 100,
            max_batch_time: Duration::from_millis(50),
            flush_on_drop: true,
        };

        let batched = mock.into_batched_with_config(config);

        batched.log(TestLog::new("INFO", "Message 1"));
        batched.log(TestLog::new("INFO", "Message 2"));

        std::thread::sleep(Duration::from_millis(100));

        let messages = mock_clone.get_messages();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].message, "Message 1");
        assert_eq!(messages[1].message, "Message 2");
    }

    #[test]
    fn test_manual_flush() {
        let mock: MockTransport<TestLog> = MockTransport::new();
        let mock_clone = mock.clone();

        let config = BatchConfig {
            max_batch_size: 100,
            max_batch_time: Duration::from_secs(10),
            flush_on_drop: true,
        };

        let batched = mock.into_batched_with_config(config);

        batched.log(TestLog::new("INFO", "Message 1"));
        batched.flush().unwrap();

        std::thread::sleep(Duration::from_millis(50));

        let messages = mock_clone.get_messages();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].message, "Message 1");
    }

    /// Generic mock transport with query support and error injection
    #[derive(Clone)]
    struct MockQueryTransport<L>
    where
        L: Clone + Send + 'static,
    {
        messages: Arc<Mutex<Vec<L>>>,
        log_calls: Arc<Mutex<Vec<Instant>>>,
        should_fail: Arc<Mutex<bool>>,
    }

    impl<L> MockQueryTransport<L>
    where
        L: Clone + Send + 'static,
    {
        fn new() -> Self {
            Self {
                messages: Arc::new(Mutex::new(Vec::new())),
                log_calls: Arc::new(Mutex::new(Vec::new())),
                should_fail: Arc::new(Mutex::new(false)),
            }
        }

        fn fail(&self, should_fail: bool) {
            *self.should_fail.lock().unwrap() = should_fail;
        }
    }

    impl<L> Transport<L> for MockQueryTransport<L>
    where
        L: Clone + Send + 'static,
    {
        fn log(&self, info: L) {
            self.messages.lock().unwrap().push(info);
            self.log_calls.lock().unwrap().push(Instant::now());
        }

        fn log_batch(&self, batch: Vec<L>) {
            let mut messages = self.messages.lock().unwrap();
            let mut log_calls = self.log_calls.lock().unwrap();

            for info in batch {
                messages.push(info);
                log_calls.push(Instant::now());
            }
        }

        fn flush(&self) -> Result<(), String> {
            if *self.should_fail.lock().unwrap() {
                Err("Flush failed".to_string())
            } else {
                Ok(())
            }
        }

        fn query(&self, _options: &LogQuery) -> Result<Vec<L>, String> {
            if *self.should_fail.lock().unwrap() {
                Err("Query failed".to_string())
            } else {
                // Return a dummy vector of logs for testing
                let logs = self.messages.lock().unwrap().clone();
                Ok(logs)
            }
        }
    }

    #[test]
    fn test_query_functionality() {
        let mock: MockQueryTransport<TestLog> = MockQueryTransport::new();
        let batched = mock.clone().into_batched();

        batched.log(TestLog::new("INFO", "Test query 1"));
        batched.flush().unwrap();

        let query = LogQuery::default();
        let result = batched.query(&query).unwrap();

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].message, "Test query 1");
    }

    #[test]
    fn test_error_handling_flush() {
        let mock: MockQueryTransport<TestLog> = MockQueryTransport::new();
        mock.fail(true); // Inject failure

        let batched = mock.into_batched();

        batched.log(TestLog::new("INFO", "Should fail"));

        // Flush returns error because transport is failing
        let flush_result = batched.flush();

        assert!(flush_result.is_err());
        assert_eq!(flush_result.unwrap_err(), "Flush failed");
    }

    #[test]
    fn test_error_handling_query() {
        let mock: MockQueryTransport<TestLog> = MockQueryTransport::new();
        let batched = mock.clone().into_batched();

        mock.fail(true); // Inject failure

        let query = LogQuery::default();

        let result = batched.query(&query);

        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "Query failed");
    }

    #[test]
    fn test_concurrent_access() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let mock: MockTransport<TestLog> = MockTransport::new();
        let batched = mock.clone().into_batched_with_config(BatchConfig {
            max_batch_size: 10,
            max_batch_time: Duration::from_secs(1),
            flush_on_drop: true,
        });

        let counter = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();

        for i in 0..5 {
            let batched = batched.clone();
            let counter = counter.clone();
            handles.push(thread::spawn(move || {
                for j in 0..20 {
                    let msg = format!("Thread {} - Message {}", i, j);
                    batched.log(TestLog::new("INFO", &msg));
                    counter.fetch_add(1, Ordering::SeqCst);
                }
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }

        // Wait a bit for the batch thread to flush
        thread::sleep(Duration::from_millis(500));

        let messages = mock.get_messages();
        assert_eq!(messages.len(), counter.load(Ordering::SeqCst));
    }

    #[test]
    fn test_shutdown_behavior_with_pending_messages() {
        let mock: MockTransport<TestLog> = MockTransport::new();
        let config = BatchConfig {
            max_batch_size: 100, // large batch size to avoid automatic flush on batch count
            max_batch_time: Duration::from_secs(10), // long timeout
            flush_on_drop: true,
        };

        let batched = mock.clone().into_batched_with_config(config);

        batched.log(TestLog::new("INFO", "Pending message 1"));
        batched.log(TestLog::new("INFO", "Pending message 2"));

        // Drop batched, which should flush pending messages because flush_on_drop = true
        drop(batched);

        // Give some time for thread to join
        std::thread::sleep(Duration::from_millis(200));

        let messages = mock.get_messages();
        assert_eq!(messages.len(), 2);

        // Check messages exist
        let message_strings: Vec<String> = messages.iter().map(|m| m.message.clone()).collect();
        assert!(message_strings.contains(&"Pending message 1".to_string()));
        assert!(message_strings.contains(&"Pending message 2".to_string()));
    }

    // Test with different log types to prove complete genericity
    #[derive(Clone, Debug, PartialEq)]
    struct CustomLogType {
        timestamp: u64,
        data: String,
    }

    impl CustomLogType {
        fn new(timestamp: u64, data: &str) -> Self {
            Self {
                timestamp,
                data: data.to_string(),
            }
        }
    }

    #[test]
    fn test_with_custom_log_type() {
        let mock: MockTransport<CustomLogType> = MockTransport::new();
        let mock_clone = mock.clone();

        let batched = mock.into_batched();

        batched.log(CustomLogType::new(123456789, "Custom log entry"));
        batched.flush().unwrap();

        std::thread::sleep(Duration::from_millis(50));

        let messages = mock_clone.get_messages();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].data, "Custom log entry");
        assert_eq!(messages[0].timestamp, 123456789);
    }

    // Even test with simple strings
    #[test]
    fn test_with_string_log_type() {
        let mock: MockTransport<String> = MockTransport::new();
        let mock_clone = mock.clone();

        let batched = mock.into_batched();

        batched.log("Simple string log".to_string());
        batched.flush().unwrap();

        std::thread::sleep(Duration::from_millis(50));

        let messages = mock_clone.get_messages();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0], "Simple string log");
    }

    #[test]
    fn test_with_log_info() {
        use logform::LogInfo;

        let mock = MockTransport::new();
        let mock_clone = mock.clone();

        let batched = mock.into_batched();

        batched.log(LogInfo::new("INFO", "Simple string log"));
        batched.flush().unwrap();

        std::thread::sleep(Duration::from_millis(50));

        let messages = mock_clone.get_messages();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].message, "Simple string log");
    }
}
