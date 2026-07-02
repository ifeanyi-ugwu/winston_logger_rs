use crate::{
    logger_builder::LoggerBuilder,
    logger_options::{LoggerOptions, OverflowPolicy},
    logger_transport::{IntoLoggerTransport, LoggerTransport},
    pipeline::{
        self, build_slot, close_slots_sync, BackpressureEvent, EventSenders, Routing,
        TransportStats, TransportStatsInner, TransportStatsMap,
    },
};
use arc_swap::ArcSwap;
use futures::channel::mpsc as fmpsc;
use logform::LogInfo;
use parking_lot::RwLock;
use std::{
    collections::{HashMap, VecDeque},
    sync::{
        atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering},
        Arc, Mutex,
    },
};
use whatwg_streams::{CountQueuingStrategy, ReadableStream};
use winston_transport::{BoxedQuerySource, LogQuery, Transport};

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
        F: logform::IntoFormatPipeline,
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

    /// Advanced: override the WritableStream high-water mark. See
    /// [`LoggerTransport::with_ws_high_water_mark`] — bigger is usually worse.
    pub fn with_ws_high_water_mark(mut self, hwm: usize) -> Self {
        self.logger_transport = self.logger_transport.with_ws_high_water_mark(hwm);
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

    /// Immutable routing (slots + global format/level/levels) behind an
    /// `ArcSwap`. `log()` does one lock-free `load_full` and dispatches against
    /// it — no slot-list copy, no lock — so a `Block`-policy park can't stall
    /// admin operations. Mutations rebuild a `Routing` and `store` it.
    routing: ArcSwap<Routing>,

    /// Serialises admin operations (add/remove/configure/close) that rebuild
    /// and `store` the routing. `log()` never takes it.
    admin_lock: parking_lot::Mutex<()>,

    /// Entries logged before any transport was admitted. Also held by the
    /// `Routing` (same `Arc`); kept on the Logger for direct access from
    /// tests and any future query path.
    pub(crate) buffer: Arc<Mutex<VecDeque<Arc<LogInfo>>>>,

    is_closed: AtomicBool,

    /// Lock-free pre-filter cache; `u8::MAX` means "accept everything".
    min_required_severity_cache: AtomicU8,

    /// Snapshot of the active levels map (name → severity). Read by
    /// `is_level_enabled` to resolve an entry's severity without
    /// touching `shared_state`. Updated whenever levels change.
    levels_snapshot: RwLock<HashMap<String, u8>>,

    /// Held so `Logger::query` can spawn the per-call `ReadableStream` it
    /// drains. Same spawner the per-slot pumps use internally.
    spawn_fn: pipeline::SpawnFn,

    /// Per-transport counters, shared with each slot (same `Arc`).
    pub(crate) stats_map: TransportStatsMap,

    /// Live `BackpressureEvent` subscribers, shared with the `Routing` (same `Arc`).
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

        // Build initial slots inline (no message-passing). Each `build_slot`
        // spawns the per-slot pump task on `spawn_fn`.
        let mut slots = Vec::new();
        for (handle, transport) in options.transports.clone().unwrap_or_default() {
            if let Some(slot) = build_slot(&spawn_fn, &stats_map, handle, transport) {
                slots.push(slot);
            }
        }

        let routing = ArcSwap::from_pointee(Routing::new(
            slots,
            options.format.clone(),
            options.level.clone(),
            options.levels.clone(),
            Arc::clone(&buffer),
            Arc::clone(&event_senders),
        ));

        let severity_cache = min_required_severity.unwrap_or(u8::MAX);

        let levels_snapshot = options
            .levels
            .as_ref()
            .map(|l| l.into_iter().map(|(k, &v)| (k.clone(), v)).collect())
            .unwrap_or_default();

        Logger {
            shared_state,
            routing,
            admin_lock: parking_lot::Mutex::new(()),
            buffer,
            is_closed: AtomicBool::new(false),
            min_required_severity_cache: AtomicU8::new(severity_cache),
            levels_snapshot: RwLock::new(levels_snapshot),
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

    fn refresh_effective_levels(
        state: &mut SharedState,
        severity_cache: &AtomicU8,
        levels_snapshot: &RwLock<HashMap<String, u8>>,
    ) {
        let levels = match &state.options.levels {
            Some(l) => l,
            None => {
                state.min_required_severity = None;
                severity_cache.store(u8::MAX, Ordering::Relaxed);
                *levels_snapshot.write() = HashMap::new();
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
        *levels_snapshot.write() = levels.into_iter().map(|(k, &v)| (k.clone(), v)).collect();
    }

    /// Level check for the caller's hot path. `u8::MAX` sentinel means "no
    /// filter" and short-circuits to true without any lock. Otherwise checks
    /// the levels snapshot — a `parking_lot::RwLock` held only for the
    /// duration of one HashMap lookup on a small fixed-size map.
    pub fn is_level_enabled(&self, level: &str) -> bool {
        let min = self.min_required_severity_cache.load(Ordering::Relaxed);
        if min == u8::MAX {
            return true;
        }
        let snapshot = self.levels_snapshot.read();
        matches!(snapshot.get(level), Some(&sev) if sev <= min)
    }

    /// Sync dispatch to every slot. Under `OverflowPolicy::Block` this
    /// parks the calling thread on the slot's `Condvar` until room appears
    /// — the slowest Block slot sets the producer's rate.
    pub fn log(&self, entry: LogInfo) {
        if !self.is_level_enabled(&entry.level) {
            return;
        }
        self.routing.load_full().process_entry(Arc::new(entry));
    }

    /// Constructs and logs an entry only if the level passes the filter.
    /// The closure is never called for levels that would be discarded.
    pub fn log_lazy(&self, level: &str, f: impl FnOnce() -> LogInfo) {
        if self.is_level_enabled(level) {
            self.log(f());
        }
    }

    /// Skip the fast-level pre-filter. Useful when the caller has already
    /// decided the entry is interesting and wants the dispatch path
    /// without re-checking.
    pub fn logi(&self, entry: LogInfo) {
        self.routing.load_full().process_entry(Arc::new(entry));
    }

    /// Synchronous flush: send a `Flush` barrier into every slot's mailbox
    /// and block until every per-slot pump acks. Returns `Ok` immediately
    /// if the Logger is already closed.
    pub fn flush(&self) -> Result<(), String> {
        if self.is_closed.load(Ordering::Acquire) {
            return Ok(());
        }
        self.routing.load_full().flush_all_sync();
        Ok(())
    }

    /// Sync teardown: flush, then close every slot. Each pump runs
    /// `writer.close()` (which drives `WritableSink::close`, i.e. the
    /// sink's durable flush) before exiting.
    pub fn close(&self) {
        if self.is_closed.swap(true, Ordering::SeqCst) {
            return;
        }
        let _admin = self.admin_lock.lock();
        let cur = self.routing.load_full();
        // Flush first so any in-flight entries hit the sink before close.
        cur.flush_all_sync();
        // Publish an empty slot list, then close the old slots outside any
        // lock (close_slots_sync blocks on pump_done).
        let old_slots = cur.slots.clone();
        self.routing.store(Arc::new(cur.with_slots(Vec::new())));
        drop(cur);
        close_slots_sync(old_slots, &self.stats_map);
    }

    /// Drain past entries matching `options` from every queryable transport.
    /// Each transport that exposed a `query_handle` at registration time
    /// gets asked to open a fresh `QuerySource`; sources are
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
            // Fast path: in-memory sources skip stream task/channel overhead.
            if let Some(entries) = handle.query_sync(options) {
                results.extend(entries);
                continue;
            }
            // Slow path: real I/O sources (file, network) use ReadableStream.
            let Some(source) = handle.query(options) else {
                continue;
            };
            let spawn_for_stream = Arc::clone(&self.spawn_fn);
            let stream = ReadableStream::builder(BoxedQuerySource(source))
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
            Self::refresh_effective_levels(
                &mut state,
                &self.min_required_severity_cache,
                &self.levels_snapshot,
            );
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

        // Admit the slot: build it, then publish a routing with it appended.
        let (was_first, added) = {
            let _admin = self.admin_lock.lock();
            let cur = self.routing.load_full();
            let was_first = cur.slots.is_empty();
            let added = if let Some(slot) =
                build_slot(&self.spawn_fn, &self.stats_map, handle, logger_transport)
            {
                let mut slots = cur.slots.clone();
                slots.push(slot);
                self.routing.store(Arc::new(cur.with_slots(slots)));
                true
            } else {
                false
            };
            (was_first, added)
        };

        // First-slot admit: drain any pre-transport buffer through it.
        // Done outside the admin lock so a slow Block sink doesn't stall it.
        if was_first && added {
            self.routing.load_full().drain_buffer_to_slots();
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
                Self::refresh_effective_levels(
                    &mut state,
                    &self.min_required_severity_cache,
                    &self.levels_snapshot,
                );
            }
            removed
        };

        if !removed {
            return false;
        }

        // Publish a routing without the matching slot, then close it outside
        // the admin lock (close blocks on pump_done).
        let taken = {
            let _admin = self.admin_lock.lock();
            let cur = self.routing.load_full();
            let mut taken = None;
            let mut kept = Vec::with_capacity(cur.slots.len());
            for slot in &cur.slots {
                if taken.is_none() && slot.handle == handle {
                    taken = Some(Arc::clone(slot));
                } else {
                    kept.push(Arc::clone(slot));
                }
            }
            if taken.is_some() {
                self.routing.store(Arc::new(cur.with_slots(kept)));
            }
            taken
        };
        if let Some(slot) = taken {
            close_slots_sync(vec![slot], &self.stats_map);
        }
        true
    }

    pub fn configure(&self, new_options: Option<LoggerOptions>) {
        let default_options = LoggerOptions::default();

        // `replace_transports`: `Some(vec)` = replace slots with these; `None` = keep live slots.
        let (format, level, levels, replace_transports) = {
            let mut state = self.shared_state.write();

            match new_options {
                Some(options) => {
                    state.options.format = options
                        .format
                        .or_else(|| state.options.format.take().or(default_options.format));
                    state.options.levels = options
                        .levels
                        .or_else(|| state.options.levels.take().or(default_options.levels));
                    state.options.level = options
                        .level
                        .or_else(|| state.options.level.take().or(default_options.level));

                    // `None` means "leave transports alone"; `Some` (including empty) replaces.
                    let replace = options.transports.map(|new_transports| {
                        state.options.transports = Some(new_transports.clone());
                        state.transport_levels = new_transports
                            .iter()
                            .map(|(h, t)| (*h, t.get_level().cloned()))
                            .collect();
                        new_transports
                    });

                    Self::refresh_effective_levels(
                        &mut state,
                        &self.min_required_severity_cache,
                        &self.levels_snapshot,
                    );

                    (
                        state.options.format.clone(),
                        state.options.level.clone(),
                        state.options.levels.clone(),
                        replace,
                    )
                }
                None => {
                    // configure(None) = clear transports, preserve everything else.
                    state.options.transports = Some(Vec::new());
                    state.transport_levels = vec![];
                    Self::refresh_effective_levels(
                        &mut state,
                        &self.min_required_severity_cache,
                        &self.levels_snapshot,
                    );
                    (
                        state.options.format.clone(),
                        state.options.level.clone(),
                        state.options.levels.clone(),
                        Some(Vec::new()),
                    )
                }
            }
        };

        if let Some(new_transports) = replace_transports {
            // Pre-create stats for the new transports; stale entries get
            // cleared by close_slots_sync below.
            {
                let mut map = self.stats_map.write().unwrap();
                for (h, _) in &new_transports {
                    map.entry(*h)
                        .or_insert_with(|| Arc::new(TransportStatsInner::default()));
                }
            }

            // Build new slots, publish the new routing, then close old slots
            // outside the admin lock (close blocks on pump_done).
            let old_slots = {
                let _admin = self.admin_lock.lock();
                let cur = self.routing.load_full();
                let old_slots = cur.slots.clone();
                let mut slots = Vec::new();
                for (h, t) in new_transports {
                    if let Some(slot) = build_slot(&self.spawn_fn, &self.stats_map, h, t) {
                        slots.push(slot);
                    }
                }
                self.routing.store(Arc::new(Routing::new(
                    slots,
                    format,
                    level,
                    levels,
                    Arc::clone(&self.buffer),
                    Arc::clone(&self.event_senders),
                )));
                old_slots
            };
            close_slots_sync(old_slots, &self.stats_map);

            // Drain any pre-transport buffer through the new slot list.
            self.routing.load_full().drain_buffer_to_slots();
        } else {
            // Transports unchanged: republish routing with new format/level/levels,
            // reusing the live slot list.
            let _admin = self.admin_lock.lock();
            let cur = self.routing.load_full();
            self.routing.store(Arc::new(Routing::new(
                cur.slots.clone(),
                format,
                level,
                levels,
                Arc::clone(&self.buffer),
                Arc::clone(&self.event_senders),
            )));
        }
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
fn log_level_str(level: log::Level) -> &'static str {
    match level {
        log::Level::Error => "error",
        log::Level::Warn => "warn",
        log::Level::Info => "info",
        log::Level::Debug => "debug",
        log::Level::Trace => "trace",
    }
}

#[cfg(feature = "log-backend")]
impl Log for Logger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        self.is_level_enabled(log_level_str(metadata.level()))
    }

    fn log(&self, record: &Record) {
        let level_str = log_level_str(record.level());
        if !self.is_level_enabled(level_str) {
            return;
        }

        // Static keys stay borrowed (zero key allocation); the four common
        // fields fit inline in `Meta` without a heap spill.
        let mut meta = logform::Meta::with_capacity(4);
        meta.insert(
            "timestamp",
            serde_json::Value::String(chrono::Utc::now().to_rfc3339()),
        );
        meta.insert(
            "target",
            serde_json::Value::String(record.target().to_string()),
        );
        if let Some(file) = record.file() {
            meta.insert("file", serde_json::Value::String(file.to_string()));
        }
        if let Some(line) = record.line() {
            meta.insert(
                "line",
                serde_json::Value::Number(serde_json::Number::from(line)),
            );
        }
        if let Some(module_path) = record.module_path() {
            if module_path != record.target() {
                meta.insert(
                    "module_path",
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

        let log_info = LogInfo::from_parts(level_str.to_string(), record.args().to_string(), meta);
        self.logi(log_info);
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
    use logform::{FinalizeExt, FormattedEntry};
    use crate::logger_options::LoggerOptions;
    use futures::StreamExt;
    use std::sync::{Arc, Mutex};
    use winston_transport::{DynQueryHandle, DynQuerySource, QuerySource, TransportResult};

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

    impl Transport for TestTransport {
        async fn log(&mut self, entry: FormattedEntry) -> TransportResult<()> {
            self.logs.lock().unwrap().push(entry.info);
            Ok(())
        }

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
        fn query(&self, _options: &LogQuery) -> Option<Box<dyn DynQuerySource>> {
            let snapshot = self.logs.lock().unwrap().clone();
            Some(Box::new(VecSource {
                entries: snapshot.into_iter(),
            }))
        }

        fn query_sync(&self, _options: &LogQuery) -> Option<Vec<LogInfo>> {
            Some(self.logs.lock().unwrap().clone())
        }
    }

    struct VecSource {
        entries: std::vec::IntoIter<LogInfo>,
    }

    impl QuerySource for VecSource {
        async fn next(&mut self) -> TransportResult<Option<LogInfo>> {
            Ok(self.entries.next())
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
    fn test_with_ws_high_water_mark_stores_and_clamps() {
        let lt = LoggerTransport::new(TestTransport::new()).with_ws_high_water_mark(8);
        assert_eq!(lt.ws_high_water_mark(), Some(8));

        let clamped = LoggerTransport::new(TestTransport::new()).with_ws_high_water_mark(0);
        assert_eq!(clamped.ws_high_water_mark(), Some(1));

        // Unset by default.
        assert_eq!(LoggerTransport::new(TestTransport::new()).ws_high_water_mark(), None);
    }

    #[test]
    fn test_ws_high_water_mark_override_still_delivers() {
        // Exercises the build_slot → resolve_ws_hwm explicit-override path; an
        // override deeper than the queue_capacity is honored as-is.
        let transport = TestTransport::new();
        let lt = LoggerTransport::new(transport.clone())
            .with_queue_capacity(2)
            .with_ws_high_water_mark(64);
        let logger = Logger::builder().transport(lt).build();

        logger.log(LogInfo::new("info", "via overridden ws hwm"));
        logger.flush().unwrap();

        assert_eq!(transport.get_logs().len(), 1);
    }

    // --- Two-pass format composition (ADR 0008) ---

    /// A transform that stamps `meta[key] = value`, to observe composition.
    struct Stamp(&'static str, &'static str);
    impl logform::Format for Stamp {
        type Input = LogInfo;
        fn transform(&self, mut info: LogInfo) -> Option<LogInfo> {
            info.meta.insert(self.0, serde_json::Value::from(self.1));
            Some(info)
        }
    }

    /// A transform that counts how many times it runs, to observe Layer 1.
    struct CountingTransform(Arc<std::sync::atomic::AtomicU64>);
    impl logform::Format for CountingTransform {
        type Input = LogInfo;
        fn transform(&self, info: LogInfo) -> Option<LogInfo> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Some(info)
        }
    }

    /// A sink that captures the whole `FormattedEntry` (info + rendered), so a
    /// test can assert on `rendered`, which `TestTransport` discards.
    struct CapturingSink {
        entries: Arc<Mutex<Vec<(LogInfo, Option<String>)>>>,
    }
    impl Transport for CapturingSink {
        async fn log(&mut self, entry: FormattedEntry) -> TransportResult<()> {
            self.entries
                .lock()
                .unwrap()
                .push((entry.info, entry.rendered));
            Ok(())
        }
    }

    #[test]
    fn test_global_transform_composes_into_transport_format() {
        // Compose, not replace: a transport with its own format still receives
        // the global transform's effect.
        let transport = TestTransport::new();
        let lt = LoggerTransport::new(transport.clone())
            .with_format(Stamp("t", "transport").into_pipeline());
        let logger = Logger::builder()
            .format(Stamp("g", "global").into_pipeline())
            .transport(lt)
            .build();

        logger.log(LogInfo::new("info", "hello"));
        logger.flush().unwrap();

        let logs = transport.get_logs();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].meta.get("g").and_then(|v| v.as_str()), Some("global"));
        assert_eq!(logs[0].meta.get("t").and_then(|v| v.as_str()), Some("transport"));
    }

    #[test]
    fn test_global_transform_runs_once_across_transports() {
        // Layer 1 dedup: the global transform runs once per log(), not once per
        // transport — so a non-deterministic global transform stays consistent.
        let counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let t1 = TestTransport::new();
        let t2 = TestTransport::new();
        let logger = Logger::builder()
            .format(CountingTransform(counter.clone()).into_pipeline())
            .transport(t1.clone())
            .transport(t2.clone())
            .build();

        logger.log(LogInfo::new("info", "hello"));
        logger.flush().unwrap();

        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(t1.get_logs().len(), 1);
        assert_eq!(t2.get_logs().len(), 1);
    }

    #[test]
    fn test_transforms_only_transport_keeps_global_transforms_drops_global_finalizer() {
        // The structured-sink rule (the ⚠ cell): a transforms-only transport
        // format over a global WITH a finalizer keeps the global transforms but
        // drops the global finalizer (transport-authoritative), so `rendered` is
        // None.
        let captured = Arc::new(Mutex::new(Vec::new()));
        let lt = LoggerTransport::new(CapturingSink {
            entries: captured.clone(),
        })
        .with_format(Stamp("t", "transport").into_pipeline());
        let logger = Logger::builder()
            .format(Stamp("g", "global").finalize(logform::json()))
            .transport(lt)
            .build();

        logger.log(LogInfo::new("info", "hello"));
        logger.flush().unwrap();

        let entries = captured.lock().unwrap();
        assert_eq!(entries.len(), 1);
        let (info, rendered) = &entries[0];
        assert_eq!(info.meta.get("g").and_then(|v| v.as_str()), Some("global"));
        assert_eq!(info.meta.get("t").and_then(|v| v.as_str()), Some("transport"));
        assert!(
            rendered.is_none(),
            "structured-sink rule: the global finalizer must be dropped"
        );
    }

    #[test]
    fn test_pooled_spawner_delivers_across_transports() {
        // A logger on the bounded-pool spawner still fans out to every transport.
        let t1 = TestTransport::new();
        let t2 = TestTransport::new();
        let opts = LoggerOptions::new()
            .transport(t1.clone())
            .transport(t2.clone());
        let logger = Logger::new_with_spawner(Some(opts), crate::pooled_spawner(2));

        logger.log(LogInfo::new("info", "pooled"));
        logger.flush().unwrap();

        assert_eq!(t1.get_logs().len(), 1);
        assert_eq!(t2.get_logs().len(), 1);
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
                .format(logform::passthrough().into_pipeline()),
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
    fn test_configure_transports_none_preserves_live_slots() {
        let logger = Logger::new(Some(LoggerOptions::new().level("info")));
        let transport = TestTransport::new();
        logger.add_transport(transport.clone());

        logger.log(LogInfo::new("info", "before configure"));
        logger.flush().unwrap();
        assert_eq!(transport.get_logs().len(), 1);

        // transports: None → preserve existing transport, only change the level.
        logger.configure(Some(LoggerOptions {
            level: Some("warn".to_string()),
            transports: None,
            format: None,
            levels: None,
        }));

        // "info" should now be filtered by the new level.
        logger.log(LogInfo::new("info", "filtered after configure"));
        logger.log(LogInfo::new("warn", "passes after configure"));
        logger.flush().unwrap();

        let logs = transport.get_logs();
        assert_eq!(logs.len(), 2, "transport should still be live and received warn");
        assert_eq!(logs[1].level, "warn");
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
        let logger = Logger::builder().format(logform::passthrough().into_pipeline()).build();

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
            .with_format(logform::passthrough().into_pipeline());

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

    /// A transport that takes its write-completion permits from a
    /// channel — sustains backpressure across many writes (not just the
    /// first), unlike a single-shot oneshot gate. The test drives the
    /// rate by sending permits.
    struct PermittedTransport {
        permits: futures::channel::mpsc::UnboundedReceiver<()>,
    }

    impl Transport for PermittedTransport {
        async fn log(&mut self, _entry: FormattedEntry) -> TransportResult<()> {
            let _ = self.permits.next().await;
            Ok(())
        }
    }

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
                worker_logger.as_ref().log(LogInfo::new("info", format!("msg {}", i)));
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
                worker_logger.as_ref().log(LogInfo::new("info", format!("msg-{}", i)));
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
        impl Transport for Wrapped {
            async fn log(&mut self, entry: FormattedEntry) -> TransportResult<()> {
                let message = entry.info.message.clone();
                self.inner.log(entry).await?;
                self.seen.lock().unwrap().push(message);
                Ok(())
            }
        }

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
