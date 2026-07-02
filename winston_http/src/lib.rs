//! HTTP transport for winston.
//!
//! Sends log entries to an HTTP endpoint as JSON. With `batch_size` set, the
//! transport buffers entries and POSTs them as a JSON array when the buffer
//! fills; the leftover buffer drains during `close()`.
//!
//! # Runtime requirement
//!
//! Uses `reqwest::Client` (async), which requires a tokio reactor. Construct
//! the Logger that owns this transport with [`tokio_spawner`] so the
//! per-transport `WritableStream` task runs inside a tokio runtime:
//!
//! ```ignore
//! let logger = Logger::new_with_spawner(None, winston_http::tokio_spawner());
//! logger.add_transport(HttpTransport::builder().url("https://...").build());
//! ```

use logform::{FormattedEntry, LogInfo};
use reqwest::Client;
use serde_json::Value;
use std::{collections::HashMap, future::Future, pin::Pin, sync::Arc, time::Duration};
use winston_transport::{
    content_id, DynIngestHandle, Transport, TransportError, TransportResult,
};

/// Flatten `entry` to JSON and stamp an `_id` field with its [`content_id`].
/// The endpoint can dedup on `_id` to make a re-POST (after a crash-retry on
/// the sender side) a no-op — that's how an HTTP target becomes idempotent.
/// If the endpoint ignores `_id`, the field is harmless.
fn flat_with_id(entry: &LogInfo) -> Value {
    let mut v = entry.to_flat_value();
    if let Value::Object(ref mut map) = v {
        map.entry("_id".to_string())
            .or_insert_with(|| Value::String(content_id(entry)));
    }
    v
}

#[derive(Clone)]
pub struct HttpTransportOptions {
    pub url: String,
    pub headers: Option<HashMap<String, String>>,
    pub timeout: Option<Duration>,
    /// When set and > 1, buffer entries and POST them as an array when the
    /// buffer reaches this size. Final buffer drains during `close()`.
    pub batch_size: Option<usize>,
}

pub struct HttpTransport {
    client: Client,
    options: HttpTransportOptions,
    /// Owned by `&mut self` access through the `WritableStream` task — no Mutex.
    buffer: Vec<LogInfo>,
}

impl HttpTransport {
    pub fn new(options: HttpTransportOptions) -> Self {
        let client = Client::builder()
            .timeout(options.timeout.unwrap_or(Duration::from_secs(10)))
            .build()
            .expect("Failed to build HTTP client");

        Self {
            client,
            options,
            buffer: Vec::new(),
        }
    }

    pub fn builder() -> HttpTransportBuilder {
        HttpTransportBuilder::new()
    }

    async fn send_logs(&self, logs: &[LogInfo]) -> TransportResult<()> {
        if logs.is_empty() {
            return Ok(());
        }

        let mut request = self.client.post(&self.options.url);
        if let Some(headers) = &self.options.headers {
            for (key, value) in headers {
                request = request.header(key, value);
            }
        }

        // Single-entry payload as JSON object; multi-entry as an array —
        // matches the legacy wire format. Each entry carries an `_id` so a
        // dedup-aware endpoint can absorb retried POSTs.
        let response = if logs.len() == 1 {
            request.json(&flat_with_id(&logs[0]))
        } else {
            let flat_logs: Vec<_> = logs.iter().map(flat_with_id).collect();
            request.json(&flat_logs)
        }
        .send()
        .await
        .map_err(TransportError::other)?;

        if !response.status().is_success() {
            return Err(TransportError::from(format!(
                "HTTP error: {}",
                response.status()
            )));
        }
        Ok(())
    }
}

impl Transport for HttpTransport {
    // No query — HTTP transport doesn't keep a local log store.

    async fn log(&mut self, entry: FormattedEntry) -> TransportResult<()> {
        // HTTP ships the structured entry as JSON; the rendered string is unused.
        let info = entry.info;
        if let Some(batch_size) = self.options.batch_size {
            if batch_size > 1 {
                self.buffer.push(info);
                if self.buffer.len() >= batch_size {
                    let to_send: Vec<LogInfo> = self.buffer.drain(..).collect();
                    self.send_logs(&to_send).await?;
                }
                return Ok(());
            }
        }
        self.send_logs(&[info]).await?;
        Ok(())
    }

    async fn close(mut self) -> TransportResult<()> {
        if !self.buffer.is_empty() {
            let to_send: Vec<LogInfo> = self.buffer.drain(..).collect();
            self.send_logs(&to_send).await?;
        }
        Ok(())
    }

    fn ingest_handle(&self) -> Option<Box<dyn DynIngestHandle>> {
        Some(Box::new(HttpIngestHandle {
            client: self.client.clone(),
            url: self.options.url.clone(),
            headers: self.options.headers.clone(),
        }))
    }
}

/// Out-of-band ingest target. Each call POSTs the batch as a single JSON
/// payload — single entry as an object, multi-entry as an array — matching
/// the wire shape of the live transport's batched send.
struct HttpIngestHandle {
    client: Client,
    url: String,
    headers: Option<HashMap<String, String>>,
}

impl DynIngestHandle for HttpIngestHandle {
    fn ingest<'s>(
        &'s self,
        logs: Vec<LogInfo>,
    ) -> Pin<Box<dyn Future<Output = TransportResult<()>> + Send + 's>> {
        Box::pin(async move {
            if logs.is_empty() {
                return Ok(());
            }
            let mut request = self.client.post(&self.url);
            if let Some(headers) = &self.headers {
                for (key, value) in headers {
                    request = request.header(key, value);
                }
            }
            let response = if logs.len() == 1 {
                request.json(&flat_with_id(&logs[0]))
            } else {
                let flat: Vec<_> = logs.iter().map(flat_with_id).collect();
                request.json(&flat)
            }
            .send()
            .await
            .map_err(TransportError::other)?;

            if !response.status().is_success() {
                return Err(TransportError::from(format!(
                    "HTTP error: {}",
                    response.status()
                )));
            }
            Ok(())
        })
    }
}

pub struct HttpTransportBuilder {
    options: HttpTransportOptions,
}

impl Default for HttpTransportBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpTransportBuilder {
    pub fn new() -> Self {
        Self {
            options: HttpTransportOptions {
                url: String::new(),
                headers: None,
                timeout: None,
                batch_size: None,
            },
        }
    }

    pub fn url(mut self, url: &str) -> Self {
        self.options.url = url.to_string();
        self
    }

    pub fn headers(mut self, headers: HashMap<String, String>) -> Self {
        self.options.headers = Some(headers);
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.options.timeout = Some(timeout);
        self
    }

    pub fn batch_size(mut self, size: usize) -> Self {
        self.options.batch_size = Some(size);
        self
    }

    pub fn build(self) -> HttpTransport {
        if self.options.url.is_empty() {
            panic!("URL is required for HTTP transport");
        }
        HttpTransport::new(self.options)
    }
}


/// Returns a `SpawnFn` that schedules tasks onto the *current* tokio runtime.
///
/// Construct your `Logger` with this when registering an `HttpTransport`:
///
/// ```ignore
/// let logger = Logger::new_with_spawner(None, winston_http::tokio_spawner());
/// logger.add_transport(HttpTransport::builder().url("https://...").build());
/// ```
///
/// Must be called from inside a tokio runtime — panics otherwise.
pub fn tokio_spawner(
) -> Arc<dyn Fn(Pin<Box<dyn Future<Output = ()> + Send + 'static>>) + Send + Sync> {
    let handle = tokio::runtime::Handle::current();
    Arc::new(move |fut| {
        handle.spawn(fut);
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use logform::Format;
    use logform::timestamp;
    use serde_json::Value;
    use std::sync::Mutex;
    use std::{
        io::{BufRead, Read, Write},
        net::TcpListener,
        thread,
    };
    use whatwg_streams::{CountQueuingStrategy, WritableStream};
    use winston_transport::TransportSink;

    /// Mock HTTP server that records POSTed JSON bodies and (optionally) the
    /// request headers from each POST. Polls accept with a 5s deadline so it
    /// shuts down cleanly even if the test forgets to drain it. Binds an
    /// OS-assigned port and returns it, so concurrent tests never collide.
    fn run_mock_server(
        received_data: Arc<Mutex<Vec<Value>>>,
        last_headers: Option<Arc<Mutex<HashMap<String, String>>>>,
    ) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).expect("set_nonblocking");

        thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while std::time::Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let received_data = received_data.clone();
                        let last_headers = last_headers.clone();
                        thread::spawn(move || {
                            let mut reader = std::io::BufReader::new(&stream);
                            let mut request = String::new();
                            if reader.read_line(&mut request).is_err() {
                                return;
                            }

                            if !request.starts_with("POST") {
                                let response = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                                let _ = stream.write_all(response.as_bytes());
                                let _ = stream.flush();
                                return;
                            }

                            let mut headers = HashMap::new();
                            loop {
                                let mut line = String::new();
                                if reader.read_line(&mut line).is_err() {
                                    break;
                                }
                                if line.trim().is_empty() {
                                    break;
                                }
                                if let Some(colon_index) = line.find(':') {
                                    let key = line[..colon_index].trim().to_lowercase();
                                    let value = line[colon_index + 1..].trim().to_string();
                                    headers.insert(key, value);
                                }
                            }

                            if let Some(slot) = &last_headers {
                                if let Ok(mut guard) = slot.lock() {
                                    *guard = headers.clone();
                                }
                            }

                            let content_length = headers
                                .get("content-length")
                                .and_then(|s| s.parse::<usize>().ok())
                                .unwrap_or(0);

                            if content_length > 0 {
                                let mut body_buffer = vec![0; content_length];
                                if reader.read_exact(&mut body_buffer).is_ok() {
                                    let body = String::from_utf8_lossy(&body_buffer).to_string();
                                    if let Ok(data) = serde_json::from_str::<Value>(&body) {
                                        if let Ok(mut received) = received_data.lock() {
                                            received.push(data);
                                        }
                                    } else if let Ok(data_array) =
                                        serde_json::from_str::<Vec<Value>>(&body)
                                    {
                                        if let Ok(mut received) = received_data.lock() {
                                            received.extend(data_array);
                                        }
                                    }
                                }
                            }

                            let response = "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                            let _ = stream.write_all(response.as_bytes());
                            let _ = stream.flush();
                        });
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(20));
                    }
                    Err(_) => break,
                }
            }
        });
        port
    }

    /// Spawner that schedules tasks onto the current tokio runtime. Same shape
    /// as `tokio_spawner()` but inlined so the helper doesn't need to be
    /// exported just for tests.
    fn test_spawner<F>(fut: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        tokio::spawn(fut);
    }

    #[tokio::test]
    async fn test_single_log_http_send() {
        let received_data = Arc::new(Mutex::new(Vec::new()));
        let port = run_mock_server(received_data.clone(), None);
        let url = format!("http://127.0.0.1:{}", port);

        let transport = HttpTransport::builder().url(&url).build();
        let stream = WritableStream::builder(TransportSink(transport))
            .strategy(CountQueuingStrategy::new(8))
            .spawn(test_spawner);
        let (_locked, writer) = stream.get_writer().expect("get_writer");

        let log = timestamp()
            .transform(LogInfo::new("info", "Test single log"))
            .unwrap();
        writer.write(FormattedEntry::new(log, None)).await.expect("write");
        writer.close().await.expect("close");

        // Give server thread a moment to record the request.
        tokio::time::sleep(Duration::from_millis(100)).await;

        let received = received_data.lock().unwrap();
        assert_eq!(received.len(), 1);
        let log_entry = &received[0];
        assert_eq!(log_entry.get("level").and_then(Value::as_str), Some("info"));
        assert_eq!(
            log_entry.get("message").and_then(Value::as_str),
            Some("Test single log")
        );
    }

    #[tokio::test]
    async fn test_batched_logs_http_send() {
        let received_data = Arc::new(Mutex::new(Vec::new()));
        let port = run_mock_server(received_data.clone(), None);

        let url = format!("http://127.0.0.1:{}", port);
        let transport = HttpTransport::builder().url(&url).batch_size(2).build();
        let stream = WritableStream::builder(TransportSink(transport))
            .strategy(CountQueuingStrategy::new(8))
            .spawn(test_spawner);
        let (_locked, writer) = stream.get_writer().expect("get_writer");

        let log1 = timestamp()
            .transform(LogInfo::new("warn", "Test log 1 in batch"))
            .unwrap();
        let log2 = timestamp()
            .transform(LogInfo::new("error", "Test log 2 in batch"))
            .unwrap();
        // First two trigger a batch send (size=2).
        writer.write(FormattedEntry::new(log1, None)).await.expect("write");
        writer.write(FormattedEntry::new(log2, None)).await.expect("write");

        // Third stays buffered until close() drains it.
        let log3 = timestamp()
            .transform(LogInfo::new("info", "Test log for flush"))
            .unwrap();
        writer.write(FormattedEntry::new(log3, None)).await.expect("write");
        writer.close().await.expect("close");

        tokio::time::sleep(Duration::from_millis(200)).await;

        let received = received_data.lock().unwrap();
        assert!(!received.is_empty(), "No logs received");

        let batch = received
            .first()
            .and_then(|v| v.as_array())
            .expect("first payload should be a 2-element array");
        assert_eq!(batch.len(), 2);
        assert_eq!(
            batch[0].get("level").and_then(Value::as_str),
            Some("warn")
        );
        assert_eq!(
            batch[1].get("level").and_then(Value::as_str),
            Some("error")
        );

        let flushed = received
            .iter()
            .find(|entry| entry.get("message").and_then(Value::as_str) == Some("Test log for flush"));
        assert!(flushed.is_some(), "flush-on-close drained log not found");
    }

    #[tokio::test]
    async fn test_http_headers() {
        let received_data = Arc::new(Mutex::new(Vec::new()));
        let received_headers: Arc<Mutex<HashMap<String, String>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let port = run_mock_server(received_data.clone(), Some(received_headers.clone()));

        let url = format!("http://127.0.0.1:{}", port);
        let mut headers = HashMap::new();
        headers.insert("X-Custom-Header".to_string(), "test-value".to_string());

        let transport = HttpTransport::builder().url(&url).headers(headers).build();
        let stream = WritableStream::builder(TransportSink(transport))
            .strategy(CountQueuingStrategy::new(8))
            .spawn(test_spawner);
        let (_locked, writer) = stream.get_writer().expect("get_writer");

        let log = timestamp()
            .transform(LogInfo::new("info", "Test with custom headers"))
            .unwrap();
        writer.write(FormattedEntry::new(log, None)).await.expect("write");
        writer.close().await.expect("close");

        tokio::time::sleep(Duration::from_millis(100)).await;

        let h = received_headers.lock().unwrap();
        assert_eq!(
            h.get("x-custom-header").map(String::as_str),
            Some("test-value")
        );
        assert_eq!(
            h.get("content-type").map(String::as_str),
            Some("application/json")
        );

        let received = received_data.lock().unwrap();
        assert_eq!(received.len(), 1);
        let body = &received[0];
        assert_eq!(body.get("level").and_then(Value::as_str), Some("info"));
        assert_eq!(
            body.get("message").and_then(Value::as_str),
            Some("Test with custom headers")
        );
        assert!(
            body.get("timestamp").is_some(),
            "Timestamp should be at root level"
        );
    }

    /// Verifies `ingest_handle`: extract a handle, ingest a batch, confirm
    /// the mock server received a single batched JSON array. The live sink
    /// is not used in this path — out-of-band ingestion only.
    #[tokio::test]
    async fn ingest_handle_posts_batch() {
        let received_data = Arc::new(Mutex::new(Vec::new()));
        let port = run_mock_server(received_data.clone(), None);
        let url = format!("http://127.0.0.1:{}", port);

        let transport = HttpTransport::builder().url(&url).build();
        let handle = transport.ingest_handle().expect("ingest_handle is Some");

        let log_a = timestamp()
            .transform(LogInfo::new("info", "ingest a"))
            .unwrap();
        let log_b = timestamp()
            .transform(LogInfo::new("warn", "ingest b"))
            .unwrap();
        handle.ingest(vec![log_a, log_b]).await.expect("ingest");

        tokio::time::sleep(Duration::from_millis(100)).await;

        let received = received_data.lock().unwrap();
        let batch = received
            .first()
            .and_then(|v| v.as_array())
            .expect("ingest sends a JSON array for >1 entry");
        assert_eq!(batch.len(), 2);
        assert_eq!(
            batch[0].get("message").and_then(Value::as_str),
            Some("ingest a")
        );
        assert_eq!(
            batch[1].get("message").and_then(Value::as_str),
            Some("ingest b")
        );
    }
}
