use crate::{log_query::LogQuery, Transport};
use std::{
    marker::PhantomData,
    sync::mpsc::{self, Receiver, Sender},
    thread::{self, JoinHandle},
};

/// Message types for communicating with the background thread for ThreadedTransport
/// Generic over any log type `L`.
#[derive(Debug)]
enum TransportMessage<L> {
    Log(L),
    Flush(Sender<Result<(), String>>),
    Query(Box<LogQuery>, Sender<Result<Vec<L>, String>>),
    Shutdown,
}

/// A transport wrapper that executes all operations on a separate background thread
/// for non-blocking, asynchronous logging operations.
/// Generic over any transport type `T` and log type `L`.
pub struct ThreadedTransport<T, L>
where
    T: Transport<L> + Send + 'static,
    L: Send + 'static,
{
    sender: Sender<TransportMessage<L>>,
    thread_handle: Option<JoinHandle<()>>,
    _phantom_data: PhantomData<(T, L)>,
}

impl<T, L> ThreadedTransport<T, L>
where
    T: Transport<L> + Send + 'static,
    L: Send + 'static,
{
    /// Creates a new ThreadedTransport that wraps the given transport
    pub fn new(transport: T) -> Self {
        let (sender, receiver) = mpsc::channel();

        let thread_handle = thread::spawn(move || {
            Self::run_transport_thread(transport, receiver);
        });

        Self {
            sender,
            thread_handle: Some(thread_handle),
            _phantom_data: PhantomData,
        }
    }

    /// Creates a new ThreadedTransport with a custom thread name
    pub fn with_thread_name(transport: T, thread_name: String) -> Self {
        let (sender, receiver) = mpsc::channel();

        let thread_handle = thread::Builder::new()
            .name(thread_name)
            .spawn(move || {
                Self::run_transport_thread(transport, receiver);
            })
            .expect("Failed to spawn async transport thread");

        Self {
            sender,
            thread_handle: Some(thread_handle),
            _phantom_data: PhantomData,
        }
    }

    fn run_transport_thread(transport: T, receiver: Receiver<TransportMessage<L>>) {
        while let Ok(message) = receiver.recv() {
            match message {
                TransportMessage::Log(info) => {
                    transport.log(info);
                }
                TransportMessage::Flush(response_sender) => {
                    let result = transport.flush();
                    let _ = response_sender.send(result);
                }
                TransportMessage::Query(query, response_sender) => {
                    let result = transport.query(&query);
                    let _ = response_sender.send(result);
                }
                TransportMessage::Shutdown => {
                    let _ = transport.flush();
                    break;
                }
            }
        }
    }

    /// Gracefully shuts down the background thread
    pub fn shutdown(mut self) -> Result<(), String> {
        if let Some(handle) = self.thread_handle.take() {
            self.sender
                .send(TransportMessage::Shutdown)
                .map_err(|_| "Failed to send shutdown signal")?;

            handle
                .join()
                .map_err(|_| "Failed to join background thread")?;
        }
        Ok(())
    }
}

impl<T, L> Transport<L> for ThreadedTransport<T, L>
where
    T: Transport<L> + Send + 'static,
    L: Send + 'static,
{
    fn log(&self, info: L) {
        let _ = self.sender.send(TransportMessage::Log(info));
    }

    fn flush(&self) -> Result<(), String> {
        let (response_sender, response_receiver) = mpsc::channel();

        self.sender
            .send(TransportMessage::Flush(response_sender))
            .map_err(|_| "Failed to send flush message to background thread")?;

        response_receiver
            .recv()
            .map_err(|_| "Failed to receive flush response from background thread")?
    }

    fn query(&self, options: &LogQuery) -> Result<Vec<L>, String> {
        let (response_sender, response_receiver) = mpsc::channel();

        self.sender
            .send(TransportMessage::Query(
                Box::new(options.clone()),
                response_sender,
            ))
            .map_err(|_| "Failed to send query message to background thread")?;

        response_receiver
            .recv()
            .map_err(|_| "Failed to receive query response from background thread")?
    }
}

impl<T, L> Drop for ThreadedTransport<T, L>
where
    T: Transport<L> + Send + 'static,
    L: Send + 'static,
{
    fn drop(&mut self) {
        if let Some(handle) = self.thread_handle.take() {
            let _ = self.sender.send(TransportMessage::Shutdown);
            let _ = handle.join();
        }
    }
}

/// Extension trait for easily wrapping any transport with threaded behavior
/// Generic over any log type `L`.
pub trait IntoThreadedTransport<L>: Transport<L> + Send + Sized + 'static
where
    L: Send + 'static,
{
    /// Wraps this transport in a ThreadedTransport for non-blocking ops
    fn into_threaded(self) -> ThreadedTransport<Self, L> {
        ThreadedTransport::new(self)
    }

    /// Wraps with a custom thread name
    fn into_threaded_named(self, thread_name: String) -> ThreadedTransport<Self, L> {
        ThreadedTransport::with_thread_name(self, thread_name)
    }
}

impl<T, L> IntoThreadedTransport<L> for T
where
    T: Transport<L> + Send + Sized + 'static,
    L: Send + 'static,
{
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::{Arc, Mutex},
        thread,
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

    // Generic mock transport that works with any log type
    #[derive(Clone)]
    struct MockTransport<L>
    where
        L: Clone + Send + 'static,
    {
        messages: Arc<Mutex<Vec<L>>>,
        delay: Duration,
    }

    impl<L> MockTransport<L>
    where
        L: Clone + Send + 'static,
    {
        fn new() -> Self {
            Self {
                messages: Arc::new(Mutex::new(Vec::new())),
                delay: Duration::from_millis(0),
            }
        }

        fn with_delay(delay: Duration) -> Self {
            Self {
                messages: Arc::new(Mutex::new(Vec::new())),
                delay,
            }
        }

        fn get_messages(&self) -> Vec<L> {
            self.messages.lock().unwrap().clone()
        }
    }

    impl<L> Transport<L> for MockTransport<L>
    where
        L: Clone + Send + 'static,
    {
        fn log(&self, info: L) {
            if self.delay > Duration::from_millis(0) {
                thread::sleep(self.delay);
            }
            self.messages.lock().unwrap().push(info);
        }

        fn flush(&self) -> Result<(), String> {
            if self.delay > Duration::from_millis(0) {
                thread::sleep(self.delay);
            }
            Ok(())
        }
    }

    #[test]
    fn test_threaded_transport_basic_logging() {
        let mock: MockTransport<TestLog> = MockTransport::new();
        let mock_clone = mock.clone();
        let threaded_transport = mock.into_threaded();

        threaded_transport.log(TestLog::new("INFO", "Message 1"));
        threaded_transport.log(TestLog::new("INFO", "Message 2"));
        threaded_transport.log(TestLog::new("INFO", "Message 3"));

        threaded_transport.flush().unwrap();

        let messages = mock_clone.get_messages();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].message, "Message 1");
        assert_eq!(messages[1].message, "Message 2");
        assert_eq!(messages[2].message, "Message 3");
    }

    #[test]
    fn test_threaded_transport_non_blocking() {
        let slow_transport: MockTransport<TestLog> =
            MockTransport::with_delay(Duration::from_millis(100));
        let slow_transport_clone = slow_transport.clone();
        let threaded_transport = slow_transport.into_threaded();

        let start = std::time::Instant::now();

        threaded_transport.log(TestLog::new("INFO", "Slow message 1"));
        threaded_transport.log(TestLog::new("INFO", "Slow message 2"));

        let elapsed = start.elapsed();

        // Should be non-blocking - these calls return immediately
        assert!(elapsed < Duration::from_millis(50));

        threaded_transport.flush().unwrap();

        let messages = slow_transport_clone.get_messages();
        assert_eq!(messages.len(), 2);
    }

    #[test]
    fn test_threaded_transport_graceful_shutdown() {
        let mock: MockTransport<TestLog> = MockTransport::new();
        let mock_clone = mock.clone();
        let threaded_transport = mock.into_threaded_named("test-logger".to_string());

        threaded_transport.log(TestLog::new("INFO", "Before shutdown"));

        threaded_transport.shutdown().unwrap();

        let messages = mock_clone.get_messages();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].message, "Before shutdown");
    }

    // Test with different log types to prove complete genericity
    #[derive(Clone, Debug, PartialEq)]
    struct EventLog {
        event_type: String,
        timestamp: u64,
        user_id: Option<u32>,
        data: serde_json::Value,
    }

    impl EventLog {
        fn new(event_type: &str, timestamp: u64, user_id: Option<u32>) -> Self {
            Self {
                event_type: event_type.to_string(),
                timestamp,
                user_id,
                data: serde_json::json!({}),
            }
        }
    }

    #[test]
    fn test_with_custom_event_log() {
        let mock: MockTransport<EventLog> = MockTransport::new();
        let mock_clone = mock.clone();
        let threaded_transport = mock.into_threaded();

        threaded_transport.log(EventLog::new("user_login", 1234567890, Some(42)));
        threaded_transport.log(EventLog::new("page_view", 1234567891, Some(42)));

        threaded_transport.flush().unwrap();

        let messages = mock_clone.get_messages();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].event_type, "user_login");
        assert_eq!(messages[0].user_id, Some(42));
        assert_eq!(messages[1].event_type, "page_view");
    }

    // Test with simple string logs
    #[test]
    fn test_with_string_logs() {
        let mock: MockTransport<String> = MockTransport::new();
        let mock_clone = mock.clone();
        let threaded_transport = mock.into_threaded();

        threaded_transport.log("Simple string log 1".to_string());
        threaded_transport.log("Simple string log 2".to_string());

        threaded_transport.flush().unwrap();

        let messages = mock_clone.get_messages();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0], "Simple string log 1");
        assert_eq!(messages[1], "Simple string log 2");
    }

    #[test]
    fn test_with_log_info() {
        use logform::LogInfo;

        let mock = MockTransport::new();
        let mock_clone = mock.clone();
        let threaded_transport = mock.into_threaded();

        threaded_transport.log(LogInfo::new("INFO", "Simple string log 1"));

        threaded_transport.log(LogInfo::new("INFO", "Simple string log 2"));

        threaded_transport.flush().unwrap();

        let messages = mock_clone.get_messages();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].message, "Simple string log 1");
        assert_eq!(messages[1].message, "Simple string log 2");
    }

    // Test graceful shutdown with custom types
    #[test]
    fn test_graceful_shutdown_with_custom_types() {
        let mock: MockTransport<EventLog> = MockTransport::new();
        let mock_clone = mock.clone();
        let threaded_transport = mock.into_threaded_named("event-logger".to_string());

        threaded_transport.log(EventLog::new("app_start", 1234567890, None));
        threaded_transport.log(EventLog::new("user_action", 1234567891, Some(123)));

        // Shutdown should flush all pending messages
        threaded_transport.shutdown().unwrap();

        let messages = mock_clone.get_messages();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].event_type, "app_start");
        assert_eq!(messages[1].event_type, "user_action");
        assert_eq!(messages[1].user_id, Some(123));
    }

    // Test that Drop trait works correctly
    #[test]
    fn test_drop_behavior() {
        let mock: MockTransport<TestLog> = MockTransport::new();
        let mock_clone = mock.clone();

        {
            let threaded_transport = mock.into_threaded();
            threaded_transport.log(TestLog::new("INFO", "Will be flushed on drop"));
            // threaded_transport goes out of scope here and Drop is called
        }

        // Give some time for the drop to complete
        thread::sleep(Duration::from_millis(100));

        let messages = mock_clone.get_messages();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].message, "Will be flushed on drop");
    }
}
