use crate::{
    logger_builder::LoggerBuilder,
    logger_options::{BackpressureStrategy, LoggerOptions},
    logger_transport::{IntoLoggerTransport, LoggerTransport},
};
use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use logform::LogInfo;
use parking_lot::RwLock;
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Condvar, Mutex,
    },
    thread,
};
use winston_transport::{LogQuery, Transport};

// Static counter for generating unique transport IDs
static NEXT_TRANSPORT_ID: AtomicUsize = AtomicUsize::new(0);

/// A handle for referencing and removing transports
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct TransportHandle(usize);

impl TransportHandle {
    pub(crate) fn new() -> Self {
        TransportHandle(NEXT_TRANSPORT_ID.fetch_add(1, Ordering::Relaxed))
    }
}

/// Builder for configuring a transport before adding it to the logger
pub struct TransportBuilder<'a> {
    logger: &'a Logger,
    logger_transport: LoggerTransport<LogInfo>,
}

impl<'a> TransportBuilder<'a> {
    /// Set a custom log level for this transport
    pub fn with_level(mut self, level: impl Into<String>) -> Self {
        self.logger_transport = self.logger_transport.with_level(level);
        self
    }

    /// Set a custom format for this transport
    pub fn with_format<F>(mut self, format: F) -> Self
    where
        F: logform::Format<Input = LogInfo> + Send + Sync + 'static,
    {
        self.logger_transport = self.logger_transport.with_format(format);
        self
    }

    /// Consume the builder and add the transport to the logger, returning a handle
    pub fn add(self) -> TransportHandle {
        let handle = TransportHandle::new();

        let mut state = self.logger.shared_state.write();
        if let Some(transports) = &mut state.options.transports {
            transports.push((handle, self.logger_transport));
        } else {
            state.options.transports = Some(vec![(handle, self.logger_transport)]);
        }

        handle
    }
}

#[derive(Debug)]
pub enum LogMessage {
    Entry(Arc<LogInfo>),
    Configure(LoggerOptions),
    Shutdown,
    Flush,
}

#[derive(Debug)]
pub(crate) struct SharedState {
    pub(crate) options: LoggerOptions,
    // Cache the minimum severity needed for any transport to accept a log
    min_required_severity: Option<u8>,
}

#[derive(Debug)]
pub struct Logger {
    worker_thread: Mutex<Option<thread::JoinHandle<()>>>,
    sender: Sender<LogMessage>,
    receiver: Arc<Receiver<LogMessage>>,
    pub(crate) shared_state: Arc<RwLock<SharedState>>,
    buffer: Arc<Mutex<VecDeque<Arc<LogInfo>>>>,
    flush_complete: Arc<(Mutex<bool>, Condvar)>,
    is_closed: AtomicBool,
}

impl Logger {
    pub fn new(options: Option<LoggerOptions>) -> Self {
        let options = options.unwrap_or_default();
        let capacity = options.channel_capacity.unwrap_or(1024);
        let (sender, receiver) = bounded(capacity);
        let flush_complete = Arc::new((Mutex::new(false), Condvar::new()));

        let shared_receiver = Arc::new(receiver);
        // Pre-compute effective levels
        let min_required_severity = Self::compute_min_severity(&options);
        let shared_state = Arc::new(RwLock::new(SharedState {
            options,
            min_required_severity,
        }));

        let buffer = Arc::new(Mutex::new(VecDeque::new()));

        let worker_receiver = Arc::clone(&shared_receiver);
        let worker_shared_state = Arc::clone(&shared_state);
        let worker_buffer = Arc::clone(&buffer);
        let worker_flush_complete = Arc::clone(&flush_complete);

        // Spawn a worker thread to handle logging
        let worker_thread = thread::spawn(move || {
            Self::worker_loop(
                worker_receiver,
                worker_shared_state,
                worker_buffer,
                worker_flush_complete,
            );
        });

        Logger {
            worker_thread: Mutex::new(Some(worker_thread)),
            sender,
            shared_state,
            buffer,
            receiver: shared_receiver,
            flush_complete,
            is_closed: AtomicBool::new(false),
        }
    }

    fn compute_min_severity(options: &LoggerOptions) -> Option<u8> {
        let levels = options.levels.as_ref()?;
        let mut min_severity = options
            .level
            .as_deref()
            .and_then(|lvl| levels.get_severity(lvl));

        if let Some(transports) = &options.transports {
            for (_handle, transport) in transports {
                if let Some(transport_level) = transport.get_level() {
                    if let Some(transport_severity) = levels.get_severity(transport_level) {
                        min_severity = Some(
                            min_severity
                                .map_or(transport_severity, |cur| cur.max(transport_severity)),
                        );
                    }
                }
            }
        }

        min_severity
    }

    /// Update the cached levels when configuration changes
    fn refresh_effective_levels(state: &mut SharedState) {
        let min_required_severity = Self::compute_min_severity(&state.options);
        state.min_required_severity = min_required_severity;
    }

    fn worker_loop(
        receiver: Arc<Receiver<LogMessage>>,
        shared_state: Arc<RwLock<SharedState>>,
        buffer: Arc<Mutex<VecDeque<Arc<LogInfo>>>>,
        flush_complete: Arc<(Mutex<bool>, Condvar)>,
    ) {
        for message in receiver.iter() {
            match message {
                LogMessage::Entry(entry) => {
                    // Use read lock to check if we have transports (allows parallelism)
                    let has_transports = {
                        let state = shared_state.read();
                        state
                            .options
                            .transports
                            .as_ref()
                            .map_or(false, |t| !t.is_empty())
                    };

                    if !has_transports {
                        // Only buffer lock needed here
                        let mut buf = buffer.lock().unwrap();
                        buf.push_back(Arc::clone(&entry));
                        eprintln!(
                            "[winston] Attempt to write logs with no transports, which can increase memory usage: {}",
                            entry.message
                        );
                    } else {
                        // Process any buffered entries first
                        Self::process_buffered_entries(&shared_state, &buffer);

                        // Process current entry with read lock (allows parallel processing)
                        let state = shared_state.read();
                        Self::process_entry(&entry, &state);
                    }
                }
                LogMessage::Configure(new_options) => {
                    let mut state = shared_state.write();
                    // Update only the provided options
                    if let Some(level) = new_options.level {
                        state.options.level = Some(level);
                    }
                    if let Some(levels) = new_options.levels {
                        state.options.levels = Some(levels);
                    }
                    if let Some(transports) = new_options.transports {
                        state.options.transports = Some(transports);
                    }
                    if let Some(format) = new_options.format {
                        state.options.format = Some(format);
                    }

                    Self::refresh_effective_levels(&mut state);
                    drop(state); // Release write lock before processing buffer

                    // Process buffered entries with new configuration
                    Self::process_buffered_entries(&shared_state, &buffer);
                }
                LogMessage::Shutdown => {
                    Self::process_buffered_entries(&shared_state, &buffer);
                    break;
                }
                LogMessage::Flush => {
                    let state = shared_state.read();

                    if state
                        .options
                        .transports
                        .as_ref()
                        .map_or(false, |t| !t.is_empty())
                    {
                        drop(state); // Release read lock
                        Self::process_buffered_entries(&shared_state, &buffer);

                        let state = shared_state.read();
                        if let Some(transports) = &state.options.transports {
                            for (_handle, transport) in transports {
                                let _ = transport.get_transport().flush();
                            }
                        }
                    }

                    let (lock, cvar) = &*flush_complete;
                    let mut completed = lock.lock().unwrap();
                    *completed = true;
                    cvar.notify_one();
                }
            }
        }
    }

    fn process_buffered_entries(
        shared_state: &Arc<RwLock<SharedState>>,
        buffer: &Arc<Mutex<VecDeque<Arc<LogInfo>>>>,
    ) {
        // Drain all buffered entries at once
        let entries = {
            let mut buf = buffer.lock().unwrap();
            buf.drain(..).collect::<Vec<_>>()
        };

        if entries.is_empty() {
            return;
        }

        // Process with read lock (allows parallelism)
        let state = shared_state.read();
        for entry in entries {
            Self::process_entry(&entry, &state);
        }
    }

    fn process_entry(entry: &Arc<LogInfo>, state: &SharedState) {
        if entry.message.is_empty() && entry.meta.is_empty() {
            return;
        }

        let options = &state.options;
        if let Some(transports) = &options.transports {
            for (_handle, transport) in transports {
                // Check if this transport cares about the level
                let effective_level = transport.get_level().or_else(|| options.level.as_ref());

                if let (Some(levels), Some(effective_level)) = (&options.levels, effective_level) {
                    if let (Some(entry_sev), Some(required_sev)) = (
                        levels.get_severity(&entry.level),
                        levels.get_severity(effective_level),
                    ) {
                        if entry_sev > required_sev {
                            continue; // skip: not enabled
                        }
                    } else {
                        // If we can't get severity for either level, skip this transport
                        continue;
                    }
                }

                let formatted_message = match (transport.get_format(), &options.format) {
                    (Some(tf), Some(_lf)) => tf.transform((**entry).clone()),
                    (Some(tf), None) => tf.transform((**entry).clone()),
                    (None, Some(lf)) => lf.transform((**entry).clone()),
                    (None, None) => Some((**entry).clone()),
                };
                if let Some(msg) = formatted_message {
                    transport.get_transport().log(msg);
                }
            }
        }
    }

    fn is_level_enabled(entry_level: &str, state: &SharedState) -> bool {
        if let Some(min_required) = state.min_required_severity {
            if let Some(levels) = &state.options.levels {
                if let Some(entry_severity) = levels.get_severity(entry_level) {
                    return min_required >= entry_severity;
                }
            }
        }
        false
    }

    pub fn query(&self, options: &LogQuery) -> Result<Vec<LogInfo>, String> {
        let state = self.shared_state.read();
        let buffer = self.buffer.lock().unwrap();
        let mut results = Vec::new();

        // First, query the buffered entries
        results.extend(
            buffer
                .iter()
                .filter(|entry| options.matches(&***entry))
                .map(|arc| (**arc).clone())
                .collect::<Vec<_>>(),
        );

        // Then, query each transport
        if let Some(transports) = &state.options.transports {
            for (_handle, transport) in transports {
                match transport.get_transport().query(options) {
                    Ok(mut logs) => results.append(&mut logs),
                    Err(e) => return Err(format!("Query failed: {}", e)),
                }
            }
        }

        Ok(results)
    }

    pub fn log(&self, entry: LogInfo) {
        let entry = Arc::new(entry);
        match self.sender.try_send(LogMessage::Entry(entry)) {
            Ok(_) => {}
            Err(TrySendError::Full(LogMessage::Entry(entry))) => {
                self.handle_full_channel(entry);
            }
            Err(TrySendError::Full(LogMessage::Configure(config))) => {
                eprintln!("[winston] Channel is full, forcing config update.");
                let _ = self.sender.send(LogMessage::Configure(config));
            }
            Err(TrySendError::Full(LogMessage::Shutdown)) => {
                eprintln!("[winston] Channel is full, forcing shutdown.");
                let _ = self.sender.send(LogMessage::Shutdown);
            }
            Err(TrySendError::Full(LogMessage::Flush)) => {
                eprintln!("[winston] Channel is full, forcing flush.");
                let _ = self.sender.send(LogMessage::Flush);
            }
            Err(TrySendError::Disconnected(_)) => {
                eprintln!("[winston] Channel is disconnected. Unable to log message.");
            }
        }
    }

    pub fn logi(&self, entry: LogInfo) {
        let entry = Arc::new(entry);
        let _ = self.sender.send(LogMessage::Entry(entry));
    }

    /// Handles backpressure strategies when the channel is full.
    fn handle_full_channel(&self, entry: Arc<LogInfo>) {
        let strategy = {
            let state = self.shared_state.read();
            state
                .options
                .backpressure_strategy
                .clone()
                .unwrap_or(BackpressureStrategy::Block)
        };

        match strategy {
            BackpressureStrategy::DropOldest => {
                self.drop_oldest_and_retry(entry);
            }
            BackpressureStrategy::Block => {
                // Block until the channel has space
                let _ = self.sender.send(LogMessage::Entry(entry));
            }
            BackpressureStrategy::DropCurrent => {
                eprintln!(
                    "[winston] Dropping current log entry due to full channel: {}",
                    entry.message
                );
            }
        }
    }

    /// Drops the oldest log message from the channel and attempts to send the new one.
    fn drop_oldest_and_retry(&self, entry: Arc<LogInfo>) {
        // Try to remove the oldest message from the channel using the shared receiver
        if let Ok(oldest) = self.receiver.try_recv() {
            eprintln!(
                "[winston] Dropped oldest log entry due to full channel: {:?}",
                oldest
            );
        }

        // Now try to send the new entry again
        if let Err(e) = self.sender.try_send(LogMessage::Entry(entry)) {
            eprintln!(
                "[winston] Failed to log after dropping oldest. Dropping current message: {:?}",
                e.into_inner()
            );
        }
    }

    pub fn close(&self) {
        if self.is_closed.swap(true, Ordering::SeqCst) {
            return;
        }

        if let Err(e) = self.flush() {
            eprintln!("Error flushing logs: {}", e);
        }

        let _ = self.sender.send(LogMessage::Shutdown);

        // Wake all threads waiting on flush BEFORE joining worker
        {
            let (lock, cvar) = &*self.flush_complete;
            let mut completed = lock.lock().unwrap();
            *completed = true; // Set to true so they don't wait again
            cvar.notify_all(); // Wake ALL waiting threads
        }

        if let Ok(mut thread_handle) = self.worker_thread.lock() {
            if let Some(handle) = thread_handle.take() {
                if let Err(e) = handle.join() {
                    eprintln!("Error joining worker thread: {:?}", e);
                }
            }
        } else {
            eprintln!("Error acquiring lock on worker thread handle during close.");
        }
    }

    pub fn flush(&self) -> Result<(), String> {
        if self.is_closed.load(Ordering::Acquire) {
            return Ok(());
        }

        let (lock, cvar) = &*self.flush_complete;
        let mut completed = lock.lock().unwrap();
        *completed = false;

        // If send fails, worker is gone
        if self.sender.send(LogMessage::Flush).is_err() {
            return Ok(());
        }

        while !*completed {
            completed = cvar.wait(completed).unwrap();
        }

        Ok(())
    }

    pub fn builder() -> LoggerBuilder {
        LoggerBuilder::new()
    }

    /// Updates the logger configuration with new options, following this fallback chain:
    /// new options -> existing options -> defaults. Always clears existing transports
    /// and processes buffered entries after updating.
    ///
    /// Note: The backpressure strategy and channel capacity are not reconfigured, as they are only used during logger creation.
    ///
    /// # Arguments
    /// * `new_options` - Optional new configuration. If `None`, the existing configuration is retained.
    pub fn configure(&self, new_options: Option<LoggerOptions>) {
        let mut state = self.shared_state.write();
        let default_options = LoggerOptions::default();

        if let Some(t) = state.options.transports.as_mut() {
            t.clear();
        }

        if let Some(options) = new_options {
            state.options.format = options
                .format
                .or_else(|| state.options.format.take().or(default_options.format));

            state.options.levels = options
                .levels
                .or_else(|| state.options.levels.take().or(default_options.levels));

            state.options.level = options
                .level
                .or_else(|| state.options.level.take().or(default_options.level));

            // Add all transports we have been provided
            if let Some(transports) = options.transports {
                state.options.transports = Some(transports);
            }
        }

        Self::refresh_effective_levels(&mut state);
        drop(state); // Release write lock

        // Process buffered entries with new configuration
        Self::process_buffered_entries(&self.shared_state, &self.buffer);
    }

    /// Start building a transport configuration. Use the builder to configure
    /// level and format, then call `.add()` to add it to the logger.
    ///
    /// # Example
    /// ```ignore
    /// let handle = logger.transport(stdout())
    ///     .with_level("error")
    ///     .with_format(json())
    ///     .add();
    /// ```
    pub fn transport(
        &self,
        transport: impl Transport<LogInfo> + Send + Sync + 'static,
    ) -> TransportBuilder {
        TransportBuilder {
            logger: self,
            logger_transport: LoggerTransport::new(transport),
        }
    }

    /// Convenience method: add a transport directly without configuration.
    ///
    /// Accepts either a raw transport or a pre-configured `LoggerTransport`.
    /// Returns a handle that can be used to remove the transport later.
    ///
    /// # Example
    /// ```ignore
    /// // Raw transport
    /// let handle = logger.add_transport(stdout());
    ///
    /// // Pre-configured transport
    /// let configured = LoggerTransport::new(Arc::new(stdout()))
    ///     .with_level("error");
    /// let handle = logger.add_transport(configured);
    ///
    /// // Later...
    /// logger.remove_transport(handle);
    /// ```
    pub fn add_transport(&self, transport: impl IntoLoggerTransport) -> TransportHandle {
        let handle = TransportHandle::new();
        let logger_transport = transport.into_logger_transport();

        let mut state = self.shared_state.write();
        if let Some(transports) = &mut state.options.transports {
            transports.push((handle, logger_transport));
        } else {
            state.options.transports = Some(vec![(handle, logger_transport)]);
        }

        handle
    }

    /// Remove a transport by its handle.
    /// Returns `true` if the transport was found and removed, `false` otherwise.
    pub fn remove_transport(&self, handle: TransportHandle) -> bool {
        let mut state = self.shared_state.write();

        if let Some(transports) = &mut state.options.transports {
            if let Some(index) = transports.iter().position(|(h, _)| *h == handle) {
                transports.remove(index);
                return true;
            }
        }
        false
    }
}

impl Drop for Logger {
    fn drop(&mut self) {
        self.close();
    }
}

impl Default for Logger {
    fn default() -> Self {
        Logger::new(None)
    }
}

#[cfg(feature = "log-backend")]
use log::{Log, Metadata, Record};

#[cfg(feature = "log-backend")]
impl Log for Logger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        let state = self.shared_state.read();
        Self::is_level_enabled(&metadata.level().as_str().to_lowercase(), &state)
    }

    fn log(&self, record: &Record) {
        // Convert log::Record to LogInfo
        let mut meta = std::collections::HashMap::new();
        // Add timestamp
        meta.insert(
            "timestamp".to_string(),
            serde_json::Value::String(chrono::Utc::now().to_rfc3339()),
        );
        // Add target (module path)
        meta.insert(
            "target".to_string(),
            serde_json::Value::String(record.target().to_string()),
        );
        // Add file location if available
        if let Some(file) = record.file() {
            meta.insert(
                "file".to_string(),
                serde_json::Value::String(file.to_string()),
            );
        }
        // Add line number if available
        if let Some(line) = record.line() {
            meta.insert(
                "line".to_string(),
                serde_json::Value::Number(serde_json::Number::from(line)),
            );
        }
        // Add module path if different from target
        if let Some(module_path) = record.module_path() {
            if module_path != record.target() {
                meta.insert(
                    "module_path".to_string(),
                    serde_json::Value::String(module_path.to_string()),
                );
            }
        }

        // Add key-values if kv feature is enabled
        #[cfg(feature = "log-backend-kv")]
        {
            let mut kv_visitor = KeyValueCollector::new();
            record.key_values().visit(&mut kv_visitor).ok();

            for (key, value) in kv_visitor.collected {
                meta.insert(key, value);
            }
        }

        let log_info = LogInfo {
            level: record.level().as_str().to_lowercase(),
            message: record.args().to_string(),
            meta,
        };

        self.log(log_info);
    }

    fn flush(&self) {
        let _ = self.flush();
    }
}

#[cfg(feature = "log-backend-kv")]
struct KeyValueCollector {
    collected: Vec<(String, serde_json::Value)>,
}

#[cfg(feature = "log-backend-kv")]
impl KeyValueCollector {
    fn new() -> Self {
        Self {
            collected: Vec::new(),
        }
    }
}

#[cfg(feature = "log-backend-kv")]
impl<'kvs> log::kv::Visitor<'kvs> for KeyValueCollector {
    fn visit_pair(
        &mut self,
        key: log::kv::Key<'kvs>,
        value: log::kv::Value<'kvs>,
    ) -> Result<(), log::kv::Error> {
        let json_value = if let Some(s) = value.to_borrowed_str() {
            serde_json::Value::String(s.to_string())
        } else if let Some(i) = value.to_i64() {
            serde_json::Value::Number(serde_json::Number::from(i))
        } else if let Some(u) = value.to_u64() {
            serde_json::Value::Number(serde_json::Number::from(u))
        } else if let Some(f) = value.to_f64() {
            serde_json::Number::from_f64(f)
                .map(serde_json::Value::Number)
                .unwrap_or_else(|| serde_json::Value::String(f.to_string()))
        } else {
            // Fallback to string representation
            serde_json::Value::String(format!("{}", value))
        };

        self.collected.push((key.as_str().to_string(), json_value));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logger_options::LoggerOptions;
    use std::sync::{Arc, Mutex};

    // Simple mock for unit tests
    #[derive(Clone)]
    struct TestTransport {
        logs: Arc<Mutex<Vec<LogInfo>>>,
    }

    impl TestTransport {
        fn new() -> Self {
            Self {
                logs: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn get_logs(&self) -> Vec<LogInfo> {
            self.logs.lock().unwrap().clone()
        }
    }

    impl Transport<LogInfo> for TestTransport {
        fn log(&self, info: LogInfo) {
            self.logs.lock().unwrap().push(info);
        }

        fn flush(&self) -> Result<(), String> {
            Ok(())
        }

        fn query(&self, _: &LogQuery) -> Result<Vec<LogInfo>, String> {
            Ok(self.get_logs())
        }
    }

    #[test]
    fn test_logger_creation_with_default_options() {
        let logger = Logger::new(None);
        assert!(logger.shared_state.read().options.levels.is_some());
    }

    #[test]
    fn test_logger_creation_with_custom_options() {
        let options = LoggerOptions::new().level("debug").channel_capacity(512);

        let logger = Logger::new(Some(options));
        let state = logger.shared_state.read();
        assert_eq!(state.options.level.as_deref(), Some("debug"));
    }

    #[test]
    fn test_add_transport() {
        let logger = Logger::new(None);
        let transport = TestTransport::new();

        let handle = logger.add_transport(transport);

        {
            let state = logger.shared_state.read();
            assert_eq!(state.options.transports.as_ref().unwrap().len(), 1);
        }

        // Verify the handle works
        assert!(logger.remove_transport(handle));
    }

    #[test]
    fn test_add_multiple_transports() {
        let logger = Logger::new(None);

        let handle1 = logger.add_transport(TestTransport::new());
        let handle2 = logger.add_transport(TestTransport::new());

        let state = logger.shared_state.read();
        assert_eq!(state.options.transports.as_ref().unwrap().len(), 2);

        // Verify handles are different
        assert_ne!(handle1, handle2);
    }

    #[test]
    fn test_remove_transport() {
        let logger = Logger::new(None);
        let handle = logger.add_transport(TestTransport::new());

        assert!(logger.remove_transport(handle));

        let state = logger.shared_state.read();
        assert!(state.options.transports.as_ref().unwrap().is_empty());
    }

    #[test]
    fn test_remove_nonexistent_transport() {
        let logger = Logger::new(None);
        let fake_handle = TransportHandle(9999);

        assert!(!logger.remove_transport(fake_handle));
    }

    #[test]
    fn test_remove_transport_twice() {
        let logger = Logger::new(None);
        let handle = logger.add_transport(TestTransport::new());

        assert!(logger.remove_transport(handle));
        assert!(!logger.remove_transport(handle));
    }

    #[test]
    fn test_transport_builder() {
        let logger = Logger::new(None);
        let transport = TestTransport::new();

        let handle = logger
            .transport(transport.clone())
            .with_level("error")
            .add();

        logger.log(LogInfo::new("info", "Should be filtered"));
        logger.log(LogInfo::new("error", "Should pass"));
        logger.flush().unwrap();

        let logs = transport.get_logs();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].level, "error");

        assert!(logger.remove_transport(handle));
    }

    #[test]
    fn test_level_filtering_blocks_lower_severity() {
        let logger = Logger::new(Some(LoggerOptions::new().level("warn")));
        let transport = TestTransport::new();
        logger.add_transport(transport.clone());

        logger.log(LogInfo::new("info", "Should be filtered"));
        logger.log(LogInfo::new("debug", "Should be filtered"));
        logger.log(LogInfo::new("warn", "Should pass"));
        logger.log(LogInfo::new("error", "Should pass"));
        logger.flush().unwrap();

        let logs = transport.get_logs();
        assert_eq!(logs.len(), 2);
        assert_eq!(logs[0].level, "warn");
        assert_eq!(logs[1].level, "error");
    }

    #[test]
    fn test_level_filtering_with_trace() {
        let logger = Logger::new(Some(LoggerOptions::new().level("trace")));
        let transport = TestTransport::new();
        logger.add_transport(transport.clone());

        logger.log(LogInfo::new("trace", "Should pass"));
        logger.log(LogInfo::new("debug", "Should pass"));
        logger.log(LogInfo::new("info", "Should pass"));
        logger.flush().unwrap();

        let logs = transport.get_logs();
        assert_eq!(logs.len(), 3);
    }

    #[test]
    fn test_transport_specific_level() {
        let logger = Logger::new(Some(
            LoggerOptions::new()
                .level("trace")
                .format(logform::passthrough()),
        ));

        let transport = TestTransport::new();

        // Add transport with custom error-only level
        let _handle = logger
            .transport(transport.clone())
            .with_level("error")
            .add();

        logger.log(LogInfo::new("info", "Filtered by transport"));
        logger.log(LogInfo::new("error", "Passes transport filter"));
        logger.flush().unwrap();

        let logs = transport.get_logs();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].level, "error");
        assert_eq!(logs[0].message, "Passes transport filter");
    }

    #[test]
    fn test_empty_message_handling() {
        let logger = Logger::new(None);
        let transport = TestTransport::new();
        logger.add_transport(transport.clone());

        logger.log(LogInfo::new("info", ""));
        logger.flush().unwrap();

        // Empty messages should be filtered out
        let logs = transport.get_logs();
        assert_eq!(logs.len(), 0);
    }

    #[test]
    fn test_configure_updates_level() {
        let logger = Logger::new(Some(LoggerOptions::new().level("error")));
        let transport = TestTransport::new();
        logger.add_transport(transport.clone());

        logger.log(LogInfo::new("warn", "Should be filtered"));
        logger.flush().unwrap();
        assert_eq!(transport.get_logs().len(), 0);

        // Reconfigure to debug
        logger.configure(Some(LoggerOptions::new().level("debug")));
        logger.add_transport(transport.clone());

        logger.log(LogInfo::new("warn", "Should pass now"));
        logger.flush().unwrap();
        assert_eq!(transport.get_logs().len(), 1);
    }

    #[test]
    fn test_configure_clears_transports() {
        let logger = Logger::new(None);
        logger.add_transport(TestTransport::new());

        let state = logger.shared_state.read();
        assert_eq!(state.options.transports.as_ref().unwrap().len(), 1);
        drop(state);

        logger.configure(Some(LoggerOptions::new()));

        let state = logger.shared_state.read();
        assert!(state.options.transports.as_ref().unwrap().is_empty());
    }

    #[test]
    fn test_flush_returns_ok() {
        let logger = Logger::new(None);
        assert!(logger.flush().is_ok());
    }

    #[test]
    fn test_flush_with_transport() {
        let logger = Logger::new(None);
        let transport = TestTransport::new();
        logger.add_transport(transport.clone());

        logger.log(LogInfo::new("info", "Test"));
        assert!(logger.flush().is_ok());
        assert_eq!(transport.get_logs().len(), 1);
    }

    #[test]
    fn test_close_flushes_logs() {
        let logger = Logger::new(None);
        let transport = TestTransport::new();
        logger.add_transport(transport.clone());

        logger.log(LogInfo::new("info", "Test"));
        logger.close();

        assert_eq!(transport.get_logs().len(), 1);
    }

    #[test]
    fn test_buffering_without_transports() {
        let logger = Logger::new(None);

        logger.log(LogInfo::new("info", "Buffered message"));

        logger.flush().unwrap();

        let buffer = logger.buffer.lock().unwrap();
        assert_eq!(buffer.len(), 1);
    }

    #[test]
    fn test_buffer_processed_when_transport_added() {
        let logger = Logger::builder().format(logform::passthrough()).build();

        // Log without transport - should buffer
        logger.log(LogInfo::new("info", "Buffered"));

        logger.flush().unwrap();
        let buffer = logger.buffer.lock().unwrap();
        assert_eq!(buffer.len(), 1);
        drop(buffer);

        // Add transport
        let transport = TestTransport::new();
        logger.add_transport(transport.clone());

        // Log another message - should process buffer + new message
        logger.log(LogInfo::new("info", "Direct"));
        logger.flush().unwrap();

        let logs = transport.get_logs();
        assert_eq!(logs.len(), 2);
        assert_eq!(logs[0].message, "Buffered");
        assert_eq!(logs[1].message, "Direct");
    }

    #[test]
    fn test_query_returns_results() {
        let logger = Logger::new(None);
        let transport = TestTransport::new();
        logger.add_transport(transport);

        logger.log(LogInfo::new("info", "Test message"));
        logger.flush().unwrap();

        let query = LogQuery::new();
        let results = logger.query(&query);
        assert!(results.is_ok());
        assert_eq!(results.unwrap().len(), 1);
    }

    #[test]
    fn test_compute_min_severity() {
        let options = LoggerOptions::new().level("warn");
        let min_sev = Logger::compute_min_severity(&options);

        assert!(min_sev.is_some());
        // warn should have higher severity value than info
        assert!(min_sev.unwrap() > 0);
    }

    #[test]
    fn test_multiple_handles_different_transports() {
        let logger = Logger::new(None);

        let transport1 = TestTransport::new();
        let transport2 = TestTransport::new();

        let handle1 = logger.add_transport(transport1.clone());
        let handle2 = logger.add_transport(transport2.clone());

        logger.log(LogInfo::new("info", "Test"));
        logger.flush().unwrap();

        assert_eq!(transport1.get_logs().len(), 1);
        assert_eq!(transport2.get_logs().len(), 1);

        // Remove first transport
        assert!(logger.remove_transport(handle1));

        logger.log(LogInfo::new("info", "Test2"));
        logger.flush().unwrap();

        // Only second transport should have the new log
        assert_eq!(transport1.get_logs().len(), 1);
        assert_eq!(transport2.get_logs().len(), 2);

        // Remove second transport
        assert!(logger.remove_transport(handle2));
    }

    #[test]
    fn test_transport_accepts_raw_transport() {
        let logger = Logger::builder().transport(TestTransport::new()).build();

        let state = logger.shared_state.read();
        assert_eq!(state.options.transports.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn test_transport_accepts_preconfigured_logger_transport() {
        let transport = TestTransport::new();

        // Pre-configure a LoggerTransport with level and format
        let configured = LoggerTransport::new(transport.clone())
            .with_level("error".to_owned())
            .with_format(logform::passthrough());

        let logger = Logger::builder()
            .transport(configured) // Pre-configured LoggerTransport
            .build();

        logger.log(LogInfo::new("info", "Should be filtered"));
        logger.log(LogInfo::new("error", "Should pass"));
        logger.flush().unwrap();

        let logs = transport.get_logs();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].level, "error");
        assert_eq!(logs[0].message, "Should pass");
    }

    #[test]
    fn test_add_transport_with_raw_transport() {
        let logger = Logger::new(None);
        let transport = TestTransport::new();

        let handle = logger.add_transport(transport.clone());

        {
            let state = logger.shared_state.read();
            assert_eq!(state.options.transports.as_ref().unwrap().len(), 1);
        }

        logger.log(LogInfo::new("info", "Test"));
        logger.flush().unwrap();

        assert_eq!(transport.get_logs().len(), 1);
        assert!(logger.remove_transport(handle));
    }

    #[test]
    fn test_add_transport_with_preconfigured_logger_transport() {
        let logger = Logger::new(None);
        let transport = TestTransport::new();

        // Pre-configure with custom level
        let configured = LoggerTransport::new(transport.clone()).with_level("error".to_owned());

        let handle = logger.add_transport(configured);

        logger.log(LogInfo::new("info", "Should be filtered"));
        logger.log(LogInfo::new("error", "Should pass"));
        logger.flush().unwrap();

        let logs = transport.get_logs();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].level, "error");

        assert!(logger.remove_transport(handle));
    }

    #[test]
    fn test_builder_transports_accepts_iterable() {
        let logger = Logger::builder()
            .transports(vec![TestTransport::new(), TestTransport::new()])
            .build();

        let state = logger.shared_state.read();
        assert_eq!(state.options.transports.as_ref().unwrap().len(), 2);
    }
}
