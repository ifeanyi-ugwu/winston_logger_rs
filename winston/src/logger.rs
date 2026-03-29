use crate::{
    logger_builder::LoggerBuilder,
    logger_options::{BackpressureStrategy, LoggerOptions},
    logger_transport::{IntoLoggerTransport, LoggerTransport},
    pipeline::{self, PipelineMessage},
};
use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use futures::channel::mpsc as fmpsc;
use logform::LogInfo;
use parking_lot::RwLock;
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering},
        Arc, Condvar, Mutex,
    },
    thread,
};
use winston_transport::{LogQuery, Transport};

static NEXT_TRANSPORT_ID: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct TransportHandle(usize);

impl TransportHandle {
    pub(crate) fn new() -> Self {
        TransportHandle(NEXT_TRANSPORT_ID.fetch_add(1, Ordering::Relaxed))
    }
}

pub struct TransportBuilder<'a> {
    logger: &'a Logger,
    logger_transport: LoggerTransport<LogInfo>,
}

impl<'a> TransportBuilder<'a> {
    pub fn with_level(mut self, level: impl Into<String>) -> Self {
        self.logger_transport = self.logger_transport.with_level(level);
        self
    }

    pub fn with_format<F>(mut self, format: F) -> Self
    where
        F: logform::Format<Input = LogInfo> + Send + Sync + 'static,
    {
        self.logger_transport = self.logger_transport.with_format(format);
        self
    }

    pub fn add(self) -> TransportHandle {
        self.logger.add_transport(self.logger_transport)
    }
}

// ── Crossbeam bridge messages ────────────────────────────────────────────────
// These flow from the sync caller → bridge thread → pipeline channel.

#[derive(Debug)]
pub enum LogMessage {
    Entry(Arc<LogInfo>),
    Shutdown,
    Flush,
}

// ── Shared state (level metadata only — transports live in FanoutSink) ───────

#[derive(Debug)]
pub(crate) struct SharedState {
    pub(crate) options: LoggerOptions,
    min_required_severity: Option<u8>,
    /// Level metadata for each active transport, used only for pre-filter
    /// cache recomputation when transports are added/removed.
    transport_levels: Vec<(TransportHandle, Option<String>)>,
}

// ── Logger ───────────────────────────────────────────────────────────────────

pub struct Logger {
    /// Sync caller interface — same as before.
    sender: Sender<LogMessage>,
    receiver: Arc<Receiver<LogMessage>>,

    pub(crate) shared_state: Arc<RwLock<SharedState>>,

    /// Entries buffered when no transports are present.  Shared with FanoutSink.
    buffer: Arc<Mutex<VecDeque<Arc<LogInfo>>>>,

    flush_complete: Arc<(Mutex<bool>, Condvar)>,
    is_closed: AtomicBool,

    /// Lock-free pre-filter cache; u8::MAX means "accept everything".
    min_required_severity_cache: AtomicU8,
    backpressure_cache: AtomicU8,

    /// Sender into the async pipeline (via bridge thread).
    pipeline_tx: fmpsc::UnboundedSender<PipelineMessage>,

    bridge_thread: Mutex<Option<thread::JoinHandle<()>>>,
}

impl Logger {
    pub fn new(options: Option<LoggerOptions>) -> Self {
        Self::new_with_spawner(options, pipeline::default_spawner())
    }

    pub fn new_with_spawner(options: Option<LoggerOptions>, spawn_fn: pipeline::SpawnFn) -> Self {
        let options = options.unwrap_or_default();
        let capacity = options.channel_capacity.unwrap_or(1024);
        let (sender, receiver) = bounded::<LogMessage>(capacity);
        let flush_complete = Arc::new((Mutex::new(false), Condvar::new()));

        let shared_receiver = Arc::new(receiver);

        let min_required_severity = Self::compute_min_severity(&options);
        let bp_cache = Self::encode_backpressure(options.backpressure_strategy.as_ref());

        let transport_levels: Vec<(TransportHandle, Option<String>)> = options
            .transports
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .map(|(h, t)| (*h, t.get_level().cloned()))
            .collect();

        let shared_state = Arc::new(RwLock::new(SharedState {
            options: options.clone(),
            min_required_severity,
            transport_levels,
        }));

        let buffer = Arc::new(Mutex::new(VecDeque::new()));

        let pipeline_tx = pipeline::build_pipeline(&options, Arc::clone(&buffer), spawn_fn);

        // Bridge thread: crossbeam → pipeline channel.
        let bridge_pipeline_tx = pipeline_tx.clone();
        let bridge_flush_complete = Arc::clone(&flush_complete);
        let bridge_receiver = Arc::clone(&shared_receiver);

        let bridge_thread = thread::Builder::new()
            .name("winston-bridge".into())
            .spawn(move || {
                Self::bridge_loop(bridge_receiver, bridge_pipeline_tx, bridge_flush_complete);
            })
            .expect("failed to spawn winston bridge thread");

        let severity_cache = min_required_severity.unwrap_or(u8::MAX);

        Logger {
            sender,
            receiver: shared_receiver,
            shared_state,
            buffer,
            flush_complete,
            is_closed: AtomicBool::new(false),
            min_required_severity_cache: AtomicU8::new(severity_cache),
            backpressure_cache: AtomicU8::new(bp_cache),
            pipeline_tx,
            bridge_thread: Mutex::new(Some(bridge_thread)),
        }
    }

    // ── Bridge ────────────────────────────────────────────────────────────────

    fn bridge_loop(
        receiver: Arc<Receiver<LogMessage>>,
        pipeline_tx: fmpsc::UnboundedSender<PipelineMessage>,
        flush_complete: Arc<(Mutex<bool>, Condvar)>,
    ) {
        for msg in receiver.iter() {
            match msg {
                LogMessage::Entry(entry) => {
                    if pipeline_tx
                        .unbounded_send(PipelineMessage::Entry(entry))
                        .is_err()
                    {
                        break;
                    }
                }
                LogMessage::Flush => {
                    // Send flush into the pipeline and let FanoutSink signal
                    // the condvar when all transports have flushed.
                    let fc = Arc::clone(&flush_complete);
                    if pipeline_tx
                        .unbounded_send(PipelineMessage::Flush(fc))
                        .is_err()
                    {
                        // Pipeline gone — signal immediately so caller doesn't hang.
                        let (lock, cvar) = &*flush_complete;
                        let mut done = lock.lock().unwrap();
                        *done = true;
                        cvar.notify_one();
                    }
                }
                LogMessage::Shutdown => {
                    let _ = pipeline_tx.unbounded_send(PipelineMessage::Shutdown);
                    break;
                }
            }
        }
    }

    // ── Level helpers ─────────────────────────────────────────────────────────

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

    fn encode_backpressure(strategy: Option<&BackpressureStrategy>) -> u8 {
        match strategy.unwrap_or(&BackpressureStrategy::Block) {
            BackpressureStrategy::Block => 0,
            BackpressureStrategy::DropOldest => 1,
            BackpressureStrategy::DropCurrent => 2,
        }
    }

    fn refresh_effective_levels(state: &mut SharedState, severity_cache: &AtomicU8) {
        // Recompute using transport_levels list (not from options.transports, which
        // may be stale — the real transports live in FanoutSink).
        let levels = match &state.options.levels {
            Some(l) => l,
            None => {
                state.min_required_severity = None;
                severity_cache.store(u8::MAX, Ordering::Relaxed);
                return;
            }
        };

        let mut min_sev = state
            .options
            .level
            .as_deref()
            .and_then(|l| levels.get_severity(l));

        for (_h, transport_level) in &state.transport_levels {
            if let Some(tl) = transport_level {
                if let Some(sev) = levels.get_severity(tl) {
                    min_sev = Some(min_sev.map_or(sev, |cur: u8| cur.max(sev)));
                }
            }
        }

        state.min_required_severity = min_sev;
        severity_cache.store(min_sev.unwrap_or(u8::MAX), Ordering::Relaxed);
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

    // ── Public API ────────────────────────────────────────────────────────────

    /// Lock-free level check for use in the caller's hot path.
    ///
    /// Reads the cached min severity with a single atomic load. When no filter
    /// is configured the sentinel `u8::MAX` means "accept everything" and this
    /// returns true immediately.
    pub fn is_level_enabled_fast(&self, level: &str) -> bool {
        let min = self.min_required_severity_cache.load(Ordering::Relaxed);
        if min == u8::MAX {
            return true;
        }
        let state = self.shared_state.read();
        Self::is_level_enabled(level, &state)
    }

    pub fn log(&self, entry: LogInfo) {
        let entry = Arc::new(entry);
        match self.sender.try_send(LogMessage::Entry(entry)) {
            Ok(_) => {}
            Err(TrySendError::Full(LogMessage::Entry(entry))) => {
                self.handle_full_channel(entry);
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

    fn handle_full_channel(&self, entry: Arc<LogInfo>) {
        match self.backpressure_cache.load(Ordering::Relaxed) {
            1 => self.drop_oldest_and_retry(entry),
            2 => eprintln!(
                "[winston] Dropping current log entry due to full channel: {}",
                entry.message
            ),
            _ => {
                let _ = self.sender.send(LogMessage::Entry(entry));
            }
        }
    }

    fn drop_oldest_and_retry(&self, entry: Arc<LogInfo>) {
        if let Ok(oldest) = self.receiver.try_recv() {
            eprintln!(
                "[winston] Dropped oldest log entry due to full channel: {:?}",
                oldest
            );
        }
        if let Err(e) = self.sender.try_send(LogMessage::Entry(entry)) {
            eprintln!(
                "[winston] Failed to log after dropping oldest. Dropping current message: {:?}",
                e.into_inner()
            );
        }
    }

    pub fn flush(&self) -> Result<(), String> {
        if self.is_closed.load(Ordering::Acquire) {
            return Ok(());
        }

        let (lock, cvar) = &*self.flush_complete;
        let mut completed = lock.lock().unwrap();
        *completed = false;

        if self.sender.send(LogMessage::Flush).is_err() {
            return Ok(());
        }

        while !*completed {
            completed = cvar.wait(completed).unwrap();
        }

        Ok(())
    }

    pub fn close(&self) {
        if self.is_closed.swap(true, Ordering::SeqCst) {
            return;
        }

        // Flush inline: flush() guards against is_closed so we can't call it here.
        // The Flush message travels through the pipeline and, once FanoutSink has
        // forwarded it to every transport task and they all respond, it signals the
        // condvar — guaranteeing every queued entry is written before we proceed.
        {
            let (lock, cvar) = &*self.flush_complete;
            let mut completed = lock.lock().unwrap();
            *completed = false;
            if self.sender.send(LogMessage::Flush).is_ok() {
                while !*completed {
                    completed = cvar.wait(completed).unwrap();
                }
            }
        }

        let _ = self.sender.send(LogMessage::Shutdown);

        // Unblock any other threads that may be waiting on flush_complete.
        {
            let (lock, cvar) = &*self.flush_complete;
            let mut completed = lock.lock().unwrap();
            *completed = true;
            cvar.notify_all();
        }

        if let Ok(mut handle) = self.bridge_thread.lock() {
            if let Some(h) = handle.take() {
                let _ = h.join();
            }
        }
    }

    pub fn query(&self, options: &LogQuery) -> Result<Vec<LogInfo>, String> {
        // Query is still forwarded via shared_state transport list — transports
        // that support query need to be accessible here too. For now we keep a
        // separate Arc<dyn Transport> list just for query support.
        //
        // TODO: in a follow-up, route query requests through the pipeline.
        let state = self.shared_state.read();
        let mut results = Vec::new();

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

    // ── Dynamic transport management ─────────────────────────────────────────

    pub fn transport(
        &self,
        transport: impl Transport<LogInfo> + Send + Sync + 'static,
    ) -> TransportBuilder<'_> {
        TransportBuilder {
            logger: self,
            logger_transport: LoggerTransport::new(transport),
        }
    }

    pub fn add_transport(&self, transport: impl IntoLoggerTransport) -> TransportHandle {
        let handle = TransportHandle::new();
        let logger_transport = transport.into_logger_transport();
        let level = logger_transport.get_level().cloned();

        // Update sync-side metadata for pre-filter recomputation.
        {
            let mut state = self.shared_state.write();
            state.transport_levels.push((handle, level));
            Self::refresh_effective_levels(&mut state, &self.min_required_severity_cache);

            // Keep options.transports in sync for query() support.
            state
                .options
                .transports
                .get_or_insert_with(Vec::new)
                .push((handle, logger_transport.clone()));
        }

        // Inform the pipeline asynchronously — no round-trip needed.
        let _ = self
            .pipeline_tx
            .unbounded_send(PipelineMessage::AddTransport {
                handle,
                transport: logger_transport,
            });

        handle
    }

    pub fn remove_transport(&self, handle: TransportHandle) -> bool {
        let removed = {
            let mut state = self.shared_state.write();

            let before = state.transport_levels.len();
            state.transport_levels.retain(|(h, _)| *h != handle);
            let removed = state.transport_levels.len() < before;

            if removed {
                if let Some(transports) = &mut state.options.transports {
                    transports.retain(|(h, _)| *h != handle);
                }
                Self::refresh_effective_levels(&mut state, &self.min_required_severity_cache);
            }
            removed
        };

        if removed {
            let _ = self
                .pipeline_tx
                .unbounded_send(PipelineMessage::RemoveTransport(handle));
        }

        removed
    }

    pub fn configure(&self, new_options: Option<LoggerOptions>) {
        let default_options = LoggerOptions::default();

        let (format, level, levels, transports) = {
            let mut state = self.shared_state.write();

            // Merge options (same logic as before).
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

                if let Some(new_transports) = options.transports {
                    state.options.transports = Some(new_transports);
                } else {
                    state.options.transports = Some(Vec::new());
                }
            } else {
                state.options.transports = Some(Vec::new());
            }

            // Rebuild transport_levels from the new options.transports.
            state.transport_levels = state
                .options
                .transports
                .as_deref()
                .unwrap_or(&[])
                .iter()
                .map(|(h, t)| (*h, t.get_level().cloned()))
                .collect();

            Self::refresh_effective_levels(&mut state, &self.min_required_severity_cache);

            let transports = state.options.transports.clone().unwrap_or_default();
            (
                state.options.format.clone(),
                state.options.level.clone(),
                state.options.levels.clone(),
                transports,
            )
        };

        let _ = self.pipeline_tx.unbounded_send(PipelineMessage::Configure {
            format,
            level,
            levels,
            transports,
        });
    }

    pub fn builder() -> LoggerBuilder {
        LoggerBuilder::new()
    }
}
impl Default for Logger {
    fn default() -> Self {
        Logger::new(None)
    }
}

impl std::fmt::Debug for Logger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Logger")
            .field("is_closed", &self.is_closed)
            .finish_non_exhaustive()
    }
}

impl Drop for Logger {
    fn drop(&mut self) {
        self.close();
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
        let mut meta = std::collections::HashMap::new();
        meta.insert(
            "timestamp".to_string(),
            serde_json::Value::String(chrono::Utc::now().to_rfc3339()),
        );
        meta.insert(
            "target".to_string(),
            serde_json::Value::String(record.target().to_string()),
        );
        if let Some(file) = record.file() {
            meta.insert(
                "file".to_string(),
                serde_json::Value::String(file.to_string()),
            );
        }
        if let Some(line) = record.line() {
            meta.insert(
                "line".to_string(),
                serde_json::Value::Number(serde_json::Number::from(line)),
            );
        }
        if let Some(module_path) = record.module_path() {
            if module_path != record.target() {
                meta.insert(
                    "module_path".to_string(),
                    serde_json::Value::String(module_path.to_string()),
                );
            }
        }

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
            serde_json::Value::String(format!("{}", value))
        };
        self.collected.push((key.as_str().to_string(), json_value));
        Ok(())
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logger_options::LoggerOptions;
    use std::sync::{Arc, Mutex};

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

        assert!(logger.remove_transport(handle));
    }

    #[test]
    fn test_add_multiple_transports() {
        let logger = Logger::new(None);

        let handle1 = logger.add_transport(TestTransport::new());
        let handle2 = logger.add_transport(TestTransport::new());

        let state = logger.shared_state.read();
        assert_eq!(state.options.transports.as_ref().unwrap().len(), 2);
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

        logger.log(LogInfo::new("info", "Buffered"));
        logger.flush().unwrap();

        let buffer = logger.buffer.lock().unwrap();
        assert_eq!(buffer.len(), 1);
        drop(buffer);

        let transport = TestTransport::new();
        logger.add_transport(transport.clone());

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

        assert!(logger.remove_transport(handle1));

        logger.log(LogInfo::new("info", "Test2"));
        logger.flush().unwrap();

        assert_eq!(transport1.get_logs().len(), 1);
        assert_eq!(transport2.get_logs().len(), 2);

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

        let configured = LoggerTransport::new(transport.clone())
            .with_level("error".to_owned())
            .with_format(logform::passthrough());

        let logger = Logger::builder().transport(configured).build();

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
