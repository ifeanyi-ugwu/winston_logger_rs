use crate::{
    logger_builder::LoggerBuilder,
    logger_options::{LoggerOptions, OverflowPolicy},
    logger_transport::{IntoLoggerTransport, LoggerTransport},
    pipeline::{
        self, close_slots_sync, BackpressureEvent, EventSenders, LoggerState, TransportStats,
        TransportStatsInner, TransportStatsMap,
    },
};
use futures::channel::mpsc as fmpsc;
use logform::LogInfo;
use parking_lot::RwLock;
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering},
        Arc, Mutex,
    },
};
use whatwg_streams::{CountQueuingStrategy, ReadableStream};
use winston_transport::{BoxedReadableSource, LogQuery, Transport};

static NEXT_TRANSPORT_ID: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct TransportHandle(pub(crate) usize);

impl TransportHandle {
    pub(crate) fn new() -> Self {
        TransportHandle(NEXT_TRANSPORT_ID.fetch_add(1, Ordering::Relaxed))
    }
}

pub struct TransportBuilder<'a> {
    logger: &'a Logger,
    logger_transport: LoggerTransport,
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

    pub fn with_overflow_policy(mut self, policy: OverflowPolicy) -> Self {
        self.logger_transport = self.logger_transport.with_overflow_policy(policy);
        self
    }

    pub fn with_queue_capacity(mut self, capacity: usize) -> Self {
        self.logger_transport = self.logger_transport.with_queue_capacity(capacity);
        self
    }

    pub fn add(self) -> TransportHandle {
        self.logger.add_transport(self.logger_transport)
    }
}

#[derive(Debug)]
pub(crate) struct SharedState {
    pub(crate) options: LoggerOptions,
    min_required_severity: Option<u8>,
    /// Level metadata for each active transport, used only for pre-filter
    /// cache recomputation when transports are added/removed.
    transport_levels: Vec<(TransportHandle, Option<String>)>,
}

/// The Logger.
///
/// `log()` dispatches synchronously to per-slot mailboxes — there is no
/// caller channel, no bridge thread, no fanout task, and no pipeline channel.
/// The slowest `OverflowPolicy::Block` slot sets the producer's rate; Drop
/// policies short-circuit at their own mailbox boundary without coupling the
/// caller to that transport.
///
/// See `docs/adr/0002-direct-dispatch-backpressure.md` for the design.
pub struct Logger {
    pub(crate) shared_state: Arc<RwLock<SharedState>>,

    /// Per-slot mailboxes + format/level/levels + spawn_fn. Held behind a
    /// `RwLock` so `log()` reads a cheap `Arc`-snapshot, drops the lock, and
    /// dispatches *without* the lock — a `Block`-policy `push_blocking`
    /// parking the caller doesn't stall admin operations.
    state: Arc<RwLock<LoggerState>>,

    /// Entries logged before any transport was admitted. Also held by
    /// `LoggerState` (same `Arc`); kept on the Logger for direct access
    /// from tests and any future query path.
    pub(crate) buffer: Arc<Mutex<VecDeque<Arc<LogInfo>>>>,

    is_closed: AtomicBool,

    /// Lock-free pre-filter cache; `u8::MAX` means "accept everything".
    min_required_severity_cache: AtomicU8,

    /// Held so `Logger::query` can spawn the per-call `ReadableStream` it
    /// drains. Same spawner the per-slot pumps use internally.
    spawn_fn: pipeline::SpawnFn,

    /// Per-transport counters, shared with `LoggerState` (same `Arc`).
    pub(crate) stats_map: TransportStatsMap,

    /// Live `BackpressureEvent` subscribers, shared with `LoggerState`.
    pub(crate) event_senders: EventSenders,
}

impl Logger {
    pub fn new(options: Option<LoggerOptions>) -> Self {
        Self::new_with_spawner(options, pipeline::default_spawner())
    }

    pub fn new_with_spawner(options: Option<LoggerOptions>, spawn_fn: pipeline::SpawnFn) -> Self {
        let options = options.unwrap_or_default();

        let min_required_severity = Self::compute_min_severity(&options);

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
        let stats_map: TransportStatsMap =
            Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
        let event_senders: EventSenders = Arc::new(Mutex::new(Vec::new()));

        // Pre-create stats entries for initially-configured transports so
        // `transport_stats(handle)` returns `Some` immediately.
        {
            let mut map = stats_map.write().unwrap();
            for (h, _) in options.transports.as_deref().unwrap_or(&[]) {
                map.entry(*h)
                    .or_insert_with(|| Arc::new(TransportStatsInner::default()));
            }
        }

        let mut state = LoggerState::new(
            Arc::clone(&spawn_fn),
            options.format.clone(),
            options.level.clone(),
            options.levels.clone(),
            Arc::clone(&buffer),
            Arc::clone(&stats_map),
            Arc::clone(&event_senders),
        );

        // Admit initial transports inline (no message-passing). Each
        // `admit` spawns the per-slot pump task on `spawn_fn`.
        for (handle, transport) in options.transports.clone().unwrap_or_default() {
            state.admit(handle, transport);
        }

        let severity_cache = min_required_severity.unwrap_or(u8::MAX);

        Logger {
            shared_state,
            state: Arc::new(RwLock::new(state)),
            buffer,
            is_closed: AtomicBool::new(false),
            min_required_severity_cache: AtomicU8::new(severity_cache),
            spawn_fn,
            stats_map,
            event_senders,
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

    fn refresh_effective_levels(state: &mut SharedState, severity_cache: &AtomicU8) {
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

    /// Lock-free level check for the caller's hot path. `u8::MAX` sentinel
    /// means "no filter" and short-circuits to true.
    pub fn is_level_enabled_fast(&self, level: &str) -> bool {
        let min = self.min_required_severity_cache.load(Ordering::Relaxed);
        if min == u8::MAX {
            return true;
        }
        let state = self.shared_state.read();
        Self::is_level_enabled(level, &state)
    }

    /// Sync dispatch to every slot. Under `OverflowPolicy::Block` this
    /// parks the calling thread on the slot's `Condvar` until room appears
    /// — the slowest Block slot sets the producer's rate.
    pub fn log(&self, entry: LogInfo) {
        if !self.is_level_enabled_fast(&entry.level) {
            return;
        }
        let snapshot = self.state.read().snapshot();
        snapshot.process_entry(Arc::new(entry));
    }

    /// Constructs and logs an entry only if the level passes the filter.
    /// The closure is never called for levels that would be discarded.
    pub fn log_lazy(&self, level: &str, f: impl FnOnce() -> LogInfo) {
        if self.is_level_enabled_fast(level) {
            self.log(f());
        }
    }

    /// Skip the fast-level pre-filter. Useful when the caller has already
    /// decided the entry is interesting and wants the dispatch path
    /// without re-checking.
    pub fn logi(&self, entry: LogInfo) {
        let snapshot = self.state.read().snapshot();
        snapshot.process_entry(Arc::new(entry));
    }

    /// Synchronous flush: send a `Flush` barrier into every slot's mailbox
    /// and block until every per-slot pump acks. Returns `Ok` immediately
    /// if the Logger is already closed.
    pub fn flush(&self) -> Result<(), String> {
        if self.is_closed.load(Ordering::Acquire) {
            return Ok(());
        }
        let snapshot = self.state.read().snapshot();
        snapshot.flush_all_sync();
        Ok(())
    }

    /// Sync teardown: flush, then close every slot. Each pump runs
    /// `writer.close()` (which drives `WritableSink::close`, i.e. the
    /// sink's durable flush) before exiting.
    pub fn close(&self) {
        if self.is_closed.swap(true, Ordering::SeqCst) {
            return;
        }
        // Flush first so any in-flight entries hit the sink before close.
        let snapshot = self.state.read().snapshot();
        snapshot.flush_all_sync();
        drop(snapshot);

        // Take ownership of the slots out of state under the write lock,
        // drop the lock, then close each in parallel (close_slots_sync
        // blocks on pump_done — must not hold the state lock across it).
        let drained = {
            let mut state = self.state.write();
            state.drain_slots()
        };
        close_slots_sync(drained, &self.stats_map);
    }

    /// Drain past entries matching `options` from every queryable transport.
    /// Each transport that exposed a `query_handle` at registration time
    /// gets asked to open a fresh `ReadableSource<LogInfo>`; sources are
    /// read sequentially.
    pub async fn query(&self, options: &LogQuery) -> Result<Vec<LogInfo>, String> {
        let handles: Vec<_> = {
            let state = self.shared_state.read();
            state
                .options
                .transports
                .as_deref()
                .unwrap_or(&[])
                .iter()
                .filter_map(|(_, t)| t.query_handle().cloned())
                .collect()
        };

        let mut results = Vec::new();
        for handle in handles {
            let Some(source) = handle.query(options) else {
                continue;
            };
            let spawn_for_stream = Arc::clone(&self.spawn_fn);
            let stream = ReadableStream::builder(BoxedReadableSource(source))
                .strategy(CountQueuingStrategy::new(64))
                .spawn(move |fut| spawn_for_stream(fut));
            let (_locked, reader) = stream
                .get_reader()
                .map_err(|_| "Failed to acquire reader".to_string())?;
            loop {
                match reader.read().await {
                    Ok(Some(entry)) => results.push(entry),
                    Ok(None) => break,
                    Err(e) => return Err(format!("Query failed: {}", e)),
                }
            }
        }

        Ok(results)
    }

    pub fn transport<T>(&self, transport: T) -> TransportBuilder<'_>
    where
        T: Transport,
    {
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
            state
                .options
                .transports
                .get_or_insert_with(Vec::new)
                .push((handle, logger_transport.clone()));
        }

        // Pre-create the stats entry so `transport_stats(handle)` returns
        // `Some` immediately, without racing the pump's first push.
        self.stats_map
            .write()
            .unwrap()
            .entry(handle)
            .or_insert_with(|| Arc::new(TransportStatsInner::default()));

        // Admit the slot directly (no message-passing).
        let was_first = {
            let mut state = self.state.write();
            let was_first = state.slots.is_empty();
            state.admit(handle, logger_transport);
            was_first
        };

        // First-slot admit: drain any pre-transport buffer through it.
        // Done outside the write lock so a slow Block sink doesn't stall
        // the admin path.
        if was_first {
            let snapshot = self.state.read().snapshot();
            snapshot.drain_buffer_to_slots();
        }

        handle
    }

    /// Snapshot of the transport's lifetime counters.
    ///
    /// Returns `None` if no transport is registered under `handle`.
    pub fn transport_stats(&self, handle: TransportHandle) -> Option<TransportStats> {
        self.stats_map
            .read()
            .unwrap()
            .get(&handle)
            .map(|s| s.snapshot())
    }

    /// Subscribe to per-transport backpressure transitions.
    ///
    /// Each call returns a fresh receiver. Dropping the receiver
    /// unsubscribes (the emit path prunes dead senders on its next emit).
    /// Events are edge-triggered (full↔has-room transitions); rate stays
    /// bounded under sustained pressure.
    pub fn subscribe_backpressure(&self) -> fmpsc::UnboundedReceiver<BackpressureEvent> {
        let (tx, rx) = fmpsc::unbounded();
        self.event_senders.lock().unwrap().push(tx);
        rx
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

        if !removed {
            return false;
        }

        // Take the matching slot out under the write lock, drop the lock,
        // then close it (close blocks on pump_done — never hold a lock
        // across it).
        let taken = {
            let mut state = self.state.write();
            if let Some(pos) = state.slots.iter().position(|s| s.handle == handle) {
                Some(state.slots.remove(pos))
            } else {
                None
            }
        };
        if let Some(slot) = taken {
            close_slots_sync(vec![slot], &self.stats_map);
        }
        true
    }

    pub fn configure(&self, new_options: Option<LoggerOptions>) {
        let default_options = LoggerOptions::default();

        let (format, level, levels, transports) = {
            let mut state = self.shared_state.write();

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

        // Pre-create stats for the new transports; stale entries get
        // cleared by close_slots_sync below.
        {
            let mut map = self.stats_map.write().unwrap();
            for (h, _) in &transports {
                map.entry(*h)
                    .or_insert_with(|| Arc::new(TransportStatsInner::default()));
            }
        }

        // Drain the old slots out, update the global format/level/levels,
        // admit the new slots — all under the write lock briefly, then
        // close the old slots outside the lock.
        let old_slots = {
            let mut state = self.state.write();
            let old = state.drain_slots();
            state.global_format = format;
            state.global_level = level;
            state.levels = levels;
            for (h, t) in transports {
                state.admit(h, t);
            }
            old
        };
        close_slots_sync(old_slots, &self.stats_map);

        // Drain any pre-transport buffer through the new slot list.
        let snapshot = self.state.read().snapshot();
        snapshot.drain_buffer_to_slots();
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

        let log_info = LogInfo::from_parts(
            record.level().as_str().to_lowercase(),
            record.args().to_string(),
            meta,
        );

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


#[cfg(test)]
mod tests {
    use super::*;
    use crate::logger_options::LoggerOptions;
    use futures::StreamExt;
    use std::sync::{Arc, Mutex};
    use whatwg_streams::{
        ReadableSource, ReadableStreamDefaultController, StreamResult, WritableSink,
        WritableStreamDefaultController,
    };
    use winston_transport::{DynQueryHandle, DynReadableSource};

    /// Test transport: collects writes into a shared `Vec`. Cloning shares the
    /// underlying `Arc<Mutex<Vec<LogInfo>>>` so the test thread keeps a handle
    /// to inspect what got written, even after the Logger consumes the
    /// transport into its WritableStream.
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

    impl WritableSink<LogInfo> for TestTransport {
        async fn write(
            &mut self,
            info: LogInfo,
            _controller: &mut WritableStreamDefaultController,
        ) -> StreamResult<()> {
            self.logs.lock().unwrap().push(info);
            Ok(())
        }
    }

    impl Transport for TestTransport {
        fn query_handle(&self) -> Option<Box<dyn DynQueryHandle>> {
            Some(Box::new(TestQueryHandle {
                logs: Arc::clone(&self.logs),
            }))
        }
    }

    struct TestQueryHandle {
        logs: Arc<Mutex<Vec<LogInfo>>>,
    }

    impl DynQueryHandle for TestQueryHandle {
        fn query(&self, _options: &LogQuery) -> Option<Box<dyn DynReadableSource>> {
            let snapshot = self.logs.lock().unwrap().clone();
            Some(Box::new(VecSource {
                entries: snapshot.into_iter(),
            }))
        }
    }

    struct VecSource {
        entries: std::vec::IntoIter<LogInfo>,
    }

    impl ReadableSource<LogInfo> for VecSource {
        async fn pull(
            &mut self,
            controller: &mut ReadableStreamDefaultController<LogInfo>,
        ) -> StreamResult<()> {
            match self.entries.next() {
                Some(entry) => {
                    let _ = controller.enqueue(entry);
                }
                None => {
                    let _ = controller.close();
                }
            }
            Ok(())
        }
    }

    #[test]
    fn test_logger_creation_with_default_options() {
        let logger = Logger::new(None);
        assert!(logger.shared_state.read().options.levels.is_some());
    }

    #[test]
    fn test_logger_creation_with_custom_options() {
        let options = LoggerOptions::new().level("debug");
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
        let results = futures::executor::block_on(logger.query(&query));
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

    /// Verifies query aggregates entries across multiple registered transports.
    #[test]
    fn test_query_aggregates_multiple_transports() {
        let logger = Logger::new(None);
        let t1 = TestTransport::new();
        let t2 = TestTransport::new();
        logger.add_transport(t1.clone());
        logger.add_transport(t2.clone());

        logger.log(LogInfo::new("info", "first"));
        logger.log(LogInfo::new("info", "second"));
        logger.flush().unwrap();

        let results =
            futures::executor::block_on(logger.query(&LogQuery::new())).unwrap();
        // Each transport saw both entries; query drains both sources.
        assert_eq!(results.len(), 4);
    }

    #[test]
    fn test_transport_stats_counts_dispatched() {
        let logger = Logger::new(None);
        let handle = logger.add_transport(TestTransport::new());

        for i in 0..5 {
            logger.log(LogInfo::new("info", format!("msg {}", i)));
        }
        logger.flush().unwrap();

        let stats = logger.transport_stats(handle).expect("stats present");
        assert_eq!(stats.dispatched_total, 5);
        assert_eq!(stats.dropped_total, 0);
    }

    #[test]
    fn test_transport_stats_for_unknown_handle_is_none() {
        let logger = Logger::new(None);
        let fake = TransportHandle(99_999);
        assert!(logger.transport_stats(fake).is_none());
    }

    /// A WritableSink that takes its write-completion permits from a
    /// channel — sustains backpressure across many writes (not just the
    /// first), unlike a single-shot oneshot gate. The test drives the
    /// rate by sending permits.
    struct PermittedTransport {
        permits: futures::channel::mpsc::UnboundedReceiver<()>,
    }

    impl WritableSink<LogInfo> for PermittedTransport {
        async fn write(
            &mut self,
            _info: LogInfo,
            _controller: &mut WritableStreamDefaultController,
        ) -> StreamResult<()> {
            let _ = self.permits.next().await;
            Ok(())
        }
    }

    impl Transport for PermittedTransport {}

    /// RAII: closes the permit channel on drop by sending enough permits
    /// to drain whatever is in flight, so logger Drop's `close → flush`
    /// can complete even when an assertion panics mid-test.
    struct PermitGuard(futures::channel::mpsc::UnboundedSender<()>);
    impl Drop for PermitGuard {
        fn drop(&mut self) {
            for _ in 0..4096 {
                if self.0.unbounded_send(()).is_err() {
                    break;
                }
            }
        }
    }

    #[test]
    fn test_subscribe_backpressure_emits_saturated_then_recovered() {
        let (permit_tx, permits) = futures::channel::mpsc::unbounded::<()>();
        let transport = PermittedTransport { permits };

        let lt = LoggerTransport::new(transport)
            .with_queue_capacity(4)
            .with_overflow_policy(OverflowPolicy::Block);
        let logger = Logger::builder().transport(lt).build();
        let lt_handle = {
            let s = logger.shared_state.read();
            s.options.transports.as_ref().unwrap()[0].0
        };

        // PermitGuard declared after logger → drops first on panic/exit,
        // unblocking the sink so logger Drop's close completes.
        let _permit_guard = PermitGuard(permit_tx.clone());

        let mut events = logger.subscribe_backpressure();

        // Block-policy with sustained slow sink: producer parks once the
        // mailbox fills. Run logs in a worker thread so the test can keep
        // observing.
        let logger_for_producer = Arc::new(logger);
        let worker_logger = Arc::clone(&logger_for_producer);
        let producer = std::thread::spawn(move || {
            for i in 0..256 {
                worker_logger.log(LogInfo::new("info", format!("msg {}", i)));
            }
        });
        std::thread::sleep(std::time::Duration::from_millis(200));

        let mut after_fill = Vec::new();
        while let Ok(Some(ev)) = events.try_next() {
            after_fill.push(ev);
        }
        let stats = logger_for_producer.transport_stats(lt_handle);
        assert!(
            after_fill
                .iter()
                .any(|e| matches!(e, BackpressureEvent::Saturated { .. })),
            "expected Saturated; events={:?} stats={:?}",
            after_fill,
            stats
        );

        // Recovery phase: send enough permits to drain everything, then
        // give the pipeline time to flush.
        for _ in 0..512 {
            let _ = permit_tx.unbounded_send(());
        }
        let _ = producer.join();
        std::thread::sleep(std::time::Duration::from_millis(200));

        let mut after_recovery = Vec::new();
        while let Ok(Some(ev)) = events.try_next() {
            after_recovery.push(ev);
        }
        assert!(
            after_recovery
                .iter()
                .any(|e| matches!(e, BackpressureEvent::Recovered { .. })),
            "expected Recovered; events={:?}",
            after_recovery
        );
    }

    #[test]
    fn test_subscribe_backpressure_multi_subscriber() {
        let logger = Logger::new(None);
        let _r1 = logger.subscribe_backpressure();
        let _r2 = logger.subscribe_backpressure();
        assert_eq!(logger.event_senders.lock().unwrap().len(), 2);
    }

    #[test]
    fn test_dispatch_block_slot_does_not_gate_other_slots() {
        // Two slots: A is Block (slow sink, mailbox cap 1) — it WILL block
        // the caller once its mailbox is full. B is a fast TestTransport.
        // The water-flows-to-all-pipes contract: B should keep receiving
        // entries even while A is causing the caller to wait.
        let (a_permit_tx, a_permits) = futures::channel::mpsc::unbounded::<()>();
        let slow = PermittedTransport { permits: a_permits };
        let fast = TestTransport::new();

        let a_lt = LoggerTransport::new(slow)
            .with_queue_capacity(1)
            .with_overflow_policy(OverflowPolicy::Block);
        let b_lt = LoggerTransport::new(fast.clone())
            .with_queue_capacity(64)
            .with_overflow_policy(OverflowPolicy::DropNewest);
        let logger = Logger::builder()
            .transport(a_lt)
            .transport(b_lt)
            .build();
        let _permit_guard = PermitGuard(a_permit_tx.clone());

        // Fire log calls from a worker thread; B gets inspected from main
        // while A is saturated.
        let logger_for_producer = Arc::new(logger);
        let worker_logger = Arc::clone(&logger_for_producer);
        let producer = std::thread::spawn(move || {
            for i in 0..8 {
                worker_logger.log(LogInfo::new("info", format!("msg-{}", i)));
            }
        });

        // Give the producer time to run. With sequential dispatch (the
        // pre-fix behaviour) the producer would block on the very first
        // entry's push_blocking to slot A and B would see 0 entries.
        // With two-phase dispatch, B receives every entry the producer
        // makes it past phase 1 for — at least the first few, before A
        // saturates and starts blocking phase 2.
        std::thread::sleep(std::time::Duration::from_millis(150));

        // Fast slot (B, DropNewest, cap 64) must have seen entries
        // independent of A's blocking. We require at least 2 to prove the
        // gating is not happening (any number > 0 would technically prove
        // it, but >= 2 rules out "saw the very first entry then waited").
        let seen_b = fast.get_logs();
        assert!(
            seen_b.len() >= 2,
            "fast slot should have received multiple entries while \
             Block-slot A was saturated; got {} entries",
            seen_b.len()
        );

        // Release A's permits so the producer can drain and exit.
        for _ in 0..32 {
            let _ = a_permit_tx.unbounded_send(());
        }
        let _ = producer.join();
    }

    #[test]
    fn test_drop_oldest_evicts_head_and_delivers_newest() {
        let (permit_tx, permits) = futures::channel::mpsc::unbounded::<()>();
        let transport = PermittedTransport { permits };
        let inspect = Arc::new(Mutex::new(Vec::<String>::new()));

        // Wrap PermittedTransport so the sink writes are observable.
        struct Wrapped {
            inner: PermittedTransport,
            seen: Arc<Mutex<Vec<String>>>,
        }
        impl WritableSink<LogInfo> for Wrapped {
            async fn write(
                &mut self,
                info: LogInfo,
                ctrl: &mut WritableStreamDefaultController,
            ) -> StreamResult<()> {
                self.inner.write(info.clone(), ctrl).await?;
                self.seen.lock().unwrap().push(info.message);
                Ok(())
            }
        }
        impl Transport for Wrapped {}

        let lt = LoggerTransport::new(Wrapped {
            inner: transport,
            seen: Arc::clone(&inspect),
        })
        .with_queue_capacity(2)
        .with_overflow_policy(OverflowPolicy::DropOldest);
        let logger = Logger::builder().transport(lt).build();
        let lt_handle = {
            let s = logger.shared_state.read();
            s.options.transports.as_ref().unwrap()[0].0
        };
        let _permit_guard = PermitGuard(permit_tx.clone());

        // Block the sink so the mailbox saturates and DropOldest evicts.
        for i in 0..32 {
            logger.log(LogInfo::new("info", format!("msg-{:02}", i)));
        }
        std::thread::sleep(std::time::Duration::from_millis(150));

        let stats = logger
            .transport_stats(lt_handle)
            .expect("stats present");
        // DropOldest counts every eviction as a drop; with cap=2 + 32
        // entries, expect many drops as the head is repeatedly evicted.
        assert!(
            stats.dropped_total > 0,
            "expected DropOldest evictions; stats={:?}",
            stats
        );

        // Release the gate, drain, and verify the surviving entries are
        // the *newest* ones (DropOldest semantics, not DropNewest).
        for _ in 0..512 {
            let _ = permit_tx.unbounded_send(());
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
        logger.flush().unwrap();

        let seen = inspect.lock().unwrap().clone();
        assert!(!seen.is_empty(), "sink saw nothing");
        // The last entry the sink saw must be the latest one logged —
        // DropOldest would never evict the newest.
        assert_eq!(seen.last().unwrap(), "msg-31");
    }
}
