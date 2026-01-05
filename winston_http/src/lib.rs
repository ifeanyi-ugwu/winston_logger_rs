use logform::{Format, LogInfo};
use reqwest::blocking::Client;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use winston_proxy_transport::Proxy;
use winston_transport::Transport;

#[derive(Clone)]
pub struct HttpTransportOptions {
    pub url: String,
    pub headers: Option<HashMap<String, String>>,
    pub level: Option<String>,
    pub format: Option<Arc<dyn Format<Input = LogInfo> + Send + Sync>>,
    pub timeout: Option<Duration>,
    pub batch_size: Option<usize>,
}

pub struct HttpTransport {
    client: Client,
    options: HttpTransportOptions,
    buffer: Mutex<Vec<LogInfo>>,
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
            buffer: Mutex::new(Vec::new()),
        }
    }

    pub fn builder() -> HttpTransportBuilder {
        HttpTransportBuilder::new()
    }

    fn send_logs(&self, logs: &[LogInfo]) -> Result<(), String> {
        /*let formatted_logs: Vec<LogInfo> = if let Some(fmt) = &self.options.format {
            logs.iter()
                .filter_map(|log| fmt.transform(log.clone(), None))
                .collect()
        } else {
            logs.iter().cloned().collect()
        };*/
        //the formatting is handled by the logger
        let formatted_logs = logs;

        if formatted_logs.is_empty() {
            return Ok(());
        }

        // Build request with headers
        let mut request = self.client.post(&self.options.url);

        if let Some(headers) = &self.options.headers {
            for (key, value) in headers {
                request = request.header(key, value);
            }
        }

        // Send single log or batch of logs
        // Convert to flat representation for consistent serialization
        let response = if formatted_logs.len() == 1 {
            let flat_log = formatted_logs[0].to_flat_value();
            request.json(&flat_log)
        } else {
            let flat_logs: Vec<_> = formatted_logs
                .iter()
                .map(|log| log.to_flat_value())
                .collect();
            request.json(&flat_logs)
        }
        .send()
        .map_err(|e| format!("Failed to send log(s): {}", e))?;

        if !response.status().is_success() {
            return Err(format!("HTTP error: {}", response.status()));
        }

        Ok(())
    }
}

impl Transport<LogInfo> for HttpTransport {
    fn log(&self, info: LogInfo) {
        // If batching is enabled, buffer the log
        if let Some(batch_size) = self.options.batch_size {
            if batch_size > 1 {
                if let Ok(mut buffer) = self.buffer.lock() {
                    buffer.push(info);

                    // Send batch if we've reached the threshold
                    if buffer.len() >= batch_size {
                        let logs_to_send: Vec<LogInfo> = buffer.drain(..).collect();
                        if let Err(e) = self.send_logs(&logs_to_send) {
                            eprintln!("Failed to send log batch: {}", e);
                        }
                    }
                    return;
                }
            }
        }

        // No batching or failed to acquire lock, send immediately
        if let Err(e) = self.send_logs(&[info]) {
            eprintln!("Failed to send log: {}", e);
        }
    }

    fn log_batch(&self, logs: Vec<LogInfo>) {
        if let Err(e) = self.send_logs(&logs) {
            eprintln!("Failed to send log batch: {}", e);
        }
    }

    fn flush(&self) -> Result<(), String> {
        // Flush any buffered logs
        if let Ok(mut buffer) = self.buffer.lock() {
            if !buffer.is_empty() {
                let logs_to_send: Vec<LogInfo> = buffer.drain(..).collect();
                return self.send_logs(&logs_to_send);
            }
        }
        Ok(())
    }
}

impl Proxy<LogInfo> for HttpTransport {
    fn proxy(&self, _target: &dyn Proxy<LogInfo>) -> Result<usize, String> {
        // Returning error instead of Ok(0) here is to ensure this is only used as a target in proxying instead of as a source
        // this is because this transport typically does not hold logs locally and having it as a source does not make sense, however it fits to be used as a target
        Err("HttpTransport cannot act as a source for proxying".to_string())
    }

    fn ingest(&self, logs: Vec<LogInfo>) -> Result<(), String> {
        let formatted_logs: Vec<LogInfo> = if let Some(fmt) = &self.options.format {
            logs.iter()
                .filter_map(|log| fmt.transform(log.clone()))
                .collect()
        } else {
            logs.to_vec()
        };

        // Convert to flat representation for consistent serialization
        let flat_logs: Vec<_> = formatted_logs
            .iter()
            .map(|log| log.to_flat_value())
            .collect();

        let mut req = self.client.post(&self.options.url).json(&flat_logs);

        if let Some(headers) = &self.options.headers {
            for (k, v) in headers {
                req = req.header(k, v);
            }
        }

        let res = req.send().map_err(|e| format!("HTTP send failed: {}", e))?;

        if !res.status().is_success() {
            Err(format!("HTTP error: {}", res.status()))
        } else {
            Ok(())
        }
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
                level: None,
                format: None,
                timeout: None,
                batch_size: None,
            },
        }
    }

    pub fn url(mut self, url: &str) -> Self {
        self.options.url = url.to_string();
        self
    }

    pub fn level(mut self, level: &str) -> Self {
        self.options.level = Some(level.to_string());
        self
    }

    pub fn format(mut self, format: Arc<dyn Format<Input = LogInfo> + Send + Sync>) -> Self {
        self.options.format = Some(format);
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

#[cfg(test)]
mod tests {
    use super::*;
    use logform::timestamp;
    use serde_json::Value;
    use std::{
        io::{BufRead, Read, Write},
        net::TcpListener,
        sync::Arc,
        thread,
    };

    // A simple mock HTTP server to verify received logs
    fn run_mock_server(
        received_data: Arc<Mutex<Vec<Value>>>,
        port: u16,
    ) -> Arc<std::sync::atomic::AtomicBool> {
        println!("Starting mock server on port {}", port);
        let listener = TcpListener::bind(format!("127.0.0.1:{}", port)).unwrap();
        println!("Server bound successfully");

        // Set listener to non-blocking so it doesn't block the thread indefinitely
        listener
            .set_nonblocking(true)
            .expect("Failed to set non-blocking");

        // Create atomic flag to control server lifetime
        let running = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let running_clone = running.clone();

        thread::spawn(move || {
            while running.load(std::sync::atomic::Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let received_data = received_data.clone();
                        thread::spawn(move || {
                            println!("Processing connection in thread");
                            let mut reader = std::io::BufReader::new(&stream);
                            let mut request = String::new();
                            match reader.read_line(&mut request) {
                                Ok(_) => println!("Read request line: {}", request.trim()),
                                Err(e) => {
                                    println!("Failed to read request line: {}", e);
                                    return;
                                }
                            }

                            if request.starts_with("POST") {
                                println!("Processing POST request");
                                let mut headers = HashMap::new();
                                loop {
                                    let mut line = String::new();
                                    if let Err(e) = reader.read_line(&mut line) {
                                        println!("Error reading header: {}", e);
                                        break;
                                    }
                                    println!("Header line: {}", line.trim());
                                    if line.trim().is_empty() {
                                        break;
                                    }
                                    if let Some(colon_index) = line.find(':') {
                                        let key = line[..colon_index].trim().to_lowercase();
                                        let value = line[colon_index + 1..].trim().to_string();
                                        headers.insert(key, value);
                                    }
                                }

                                // Get content length to read correct number of bytes for body
                                let content_length = headers
                                    .get("content-length")
                                    .and_then(|s| s.parse::<usize>().ok())
                                    .unwrap_or(0);
                                println!("Content length: {}", content_length);

                                if content_length > 0 {
                                    // Read exact number of bytes specified in Content-Length
                                    let mut body_buffer = vec![0; content_length];
                                    match reader.read_exact(&mut body_buffer) {
                                        Ok(_) => {
                                            let body =
                                                String::from_utf8_lossy(&body_buffer).to_string();
                                            println!("Read body: {}", body);

                                            // Try to parse as a single JSON object
                                            if let Ok(data) = serde_json::from_str::<Value>(&body) {
                                                println!("Parsed JSON successfully");
                                                if let Ok(mut received) = received_data.lock() {
                                                    received.push(data);
                                                } else {
                                                    println!("Failed to lock received_data mutex");
                                                }
                                            }
                                            // Try to parse as a JSON array
                                            else if let Ok(data_array) =
                                                serde_json::from_str::<Vec<Value>>(&body)
                                            {
                                                println!("Parsed JSON array successfully");
                                                if let Ok(mut received) = received_data.lock() {
                                                    received.extend(data_array);
                                                } else {
                                                    println!("Failed to lock received_data mutex");
                                                }
                                            } else {
                                                println!("Failed to parse JSON body: {}", body);
                                            }
                                        }
                                        Err(e) => println!("Failed to read body: {}", e),
                                    }
                                }

                                println!("Sending 200 OK response");
                                let response = "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                                match stream.write_all(response.as_bytes()) {
                                    Ok(_) => println!("Response sent successfully"),
                                    Err(e) => println!("Failed to send response: {}", e),
                                }
                                match stream.flush() {
                                    Ok(_) => println!("Stream flushed successfully"),
                                    Err(e) => println!("Failed to flush stream: {}", e),
                                }
                            } else {
                                println!("Not a POST request, sending 404");
                                let response = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                                match stream.write_all(response.as_bytes()) {
                                    Ok(_) => println!("404 response sent"),
                                    Err(e) => println!("Failed to send 404 response: {}", e),
                                }
                                match stream.flush() {
                                    Ok(_) => println!("Stream flushed successfully"),
                                    Err(e) => println!("Failed to flush stream: {}", e),
                                }
                            }
                            println!("Finished processing request");
                        });
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        // No connections available right now, sleep a bit
                        thread::sleep(Duration::from_millis(50));
                    }
                    Err(e) => {
                        println!("Error accepting connection: {}", e);
                        break;
                    }
                }
            }
            println!("Mock server shutting down");
        });

        // Return the handle that can be used to shutdown the server
        running_clone
    }

    #[test]
    fn test_single_log_http_send() {
        let received_data = Arc::new(Mutex::new(Vec::new()));
        let port = 8081;
        let _server_running = run_mock_server(received_data.clone(), port);
        let url = format!("http://127.0.0.1:{}", port);

        let transport = HttpTransport::builder().url(&url).build();

        let log = LogInfo::new("info", "Test single log");
        transport.log(timestamp().transform(log).unwrap());

        // Give some time for the log to be sent and processed by the server
        thread::sleep(Duration::from_millis(200));

        let received = received_data.lock().unwrap();
        assert_eq!(received.len(), 1);
        if let Some(log_entry) = received.first() {
            assert_eq!(log_entry.get("level").and_then(Value::as_str), Some("info"));
            assert_eq!(
                log_entry.get("message").and_then(Value::as_str),
                Some("Test single log")
            );
        }
    }

    #[test]
    fn test_batched_logs_http_send() {
        let received_data = Arc::new(Mutex::new(Vec::new()));
        let port = 8082;

        // Start the mock server
        let _server_running = run_mock_server(received_data.clone(), port);

        // Give the server time to start
        //thread::sleep(Duration::from_millis(100));

        let url = format!("http://127.0.0.1:{}", port);
        let batch_size = 2;
        let transport = HttpTransport::builder()
            .url(&url)
            .batch_size(batch_size)
            .build();

        // Send first two logs that should be batched
        let log1 = LogInfo::new("warn", "Test log 1 in batch");
        let log2 = LogInfo::new("error", "Test log 2 in batch");
        transport.log(timestamp().transform(log1).unwrap());
        transport.log(timestamp().transform(log2).unwrap());

        // Give enough time for server to process
        thread::sleep(Duration::from_millis(500));

        // Check batch was received
        {
            let received = match received_data.lock() {
                Ok(guard) => guard,
                Err(poisoned) => {
                    println!("Mutex was poisoned, recovering");
                    poisoned.into_inner()
                }
            };

            assert!(!received.is_empty(), "No logs received");
            println!("Received {} log entries", received.len());

            if let Some(batch) = received.get(0) {
                if let Some(batch_array) = batch.as_array() {
                    assert_eq!(batch_array.len(), 2);
                    if let Some(log_entry1) = batch_array.get(0) {
                        assert_eq!(
                            log_entry1.get("level").and_then(Value::as_str),
                            Some("warn")
                        );
                        assert_eq!(
                            log_entry1.get("message").and_then(Value::as_str),
                            Some("Test log 1 in batch")
                        );
                        assert!(
                            log_entry1.get("timestamp").is_some(),
                            "Timestamp should be at root level"
                        );
                    }
                    if let Some(log_entry2) = batch_array.get(1) {
                        assert_eq!(
                            log_entry2.get("level").and_then(Value::as_str),
                            Some("error")
                        );
                        assert_eq!(
                            log_entry2.get("message").and_then(Value::as_str),
                            Some("Test log 2 in batch")
                        );
                        assert!(
                            log_entry2.get("timestamp").is_some(),
                            "Timestamp should be at root level"
                        );
                    }
                } else {
                    panic!("Received data is not an array as expected for batching");
                }
            }
        }

        // Send one more log that should be buffered
        let log3 = LogInfo::new("info", "Test log for flush");
        transport.log(timestamp().transform(log3).unwrap());

        // Give time to buffer
        //thread::sleep(Duration::from_millis(100));

        // Flush should send the buffered log
        match transport.flush() {
            Ok(_) => println!("Flush successful"),
            Err(e) => println!("Flush error (expected during tests): {}", e),
        }

        // Give server time to process
        thread::sleep(Duration::from_millis(500));

        // Check the flushed log was received
        {
            let received_after_flush = match received_data.lock() {
                Ok(guard) => guard,
                Err(poisoned) => {
                    println!("Mutex was poisoned, recovering");
                    poisoned.into_inner()
                }
            };

            assert!(
                received_after_flush.len() >= 1,
                "No logs received after flush"
            );

            // The index might vary depending on how the data was received
            // Iterate through entries to find our log
            let mut found_log = false;
            for entry in received_after_flush.iter() {
                if let Some(msg) = entry.get("message").and_then(Value::as_str) {
                    if msg == "Test log for flush" {
                        assert_eq!(entry.get("level").and_then(Value::as_str), Some("info"));
                        assert!(
                            entry.get("timestamp").is_some(),
                            "Timestamp should be at root level"
                        );
                        found_log = true;
                        break;
                    }
                }
            }

            assert!(found_log, "Could not find the flushed log in received data");
        }

        // Shutdown the server
        //server_running.store(false, std::sync::atomic::Ordering::Relaxed);

        // Give the server time to shut down cleanly
        //thread::sleep(Duration::from_millis(100));
    }

    #[test]
    fn test_http_headers() {
        let received_headers = Arc::new(Mutex::new(HashMap::new()));
        let received_headers_clone = received_headers.clone();
        let received_body = Arc::new(Mutex::new(Vec::new()));
        let received_body_clone = received_body.clone();
        let port = 8083;

        let listener = TcpListener::bind(format!("127.0.0.1:{}", port)).unwrap();
        let mock_server_handle = thread::spawn(move || {
            if let Some(Ok(mut stream)) = listener.incoming().next() {
                let mut reader = std::io::BufReader::new(&stream);
                let mut request_line = String::new();
                reader.read_line(&mut request_line).unwrap();
                let mut headers = HashMap::new();
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line.trim().is_empty() {
                        break;
                    }
                    if let Some(colon_index) = line.find(':') {
                        let key = line[..colon_index].trim().to_lowercase();
                        let value = line[colon_index + 1..].trim().to_string();
                        headers.insert(key, value);
                    }
                }
                *received_headers_clone.lock().unwrap() = headers;
                let mut body = String::new();
                reader.read_to_string(&mut body).unwrap();
                if !body.is_empty() {
                    if let Ok(data) = serde_json::from_str::<Value>(&body) {
                        received_body.lock().unwrap().push(data);
                    }
                }
                let response = "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n";
                stream.write_all(response.as_bytes()).unwrap();
                stream.flush().unwrap();
            }
        });
        let url = format!("http://127.0.0.1:{}", port);
        let mut headers = HashMap::new();
        headers.insert("X-Custom-Header".to_string(), "test-value".to_string());

        let transport = HttpTransport::builder()
            .url(&url)
            //.format(json())
            .headers(headers)
            .build();

        let log = LogInfo::new("info", "Test with custom headers");
        transport.log(timestamp().transform(log).unwrap());

        thread::sleep(Duration::from_millis(200));

        let received_h = received_headers.lock().unwrap();
        assert!(received_h.contains_key("x-custom-header"));
        assert_eq!(
            received_h.get("x-custom-header").map(|s| s.as_str()),
            Some("test-value")
        );
        assert_eq!(
            received_h.get("content-type").map(|s| s.as_str()),
            Some("application/json")
        ); // reqwest sets this

        let received_b = received_body_clone.lock().unwrap();
        assert_eq!(received_b.len(), 1);
        if let Some(body) = received_b.first() {
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

        drop(transport);
        drop(mock_server_handle);
    }
}
