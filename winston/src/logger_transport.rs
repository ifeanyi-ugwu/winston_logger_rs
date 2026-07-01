use std::{fmt, sync::Arc};

use logform::{FormatPipeline, IntoFormatPipeline};
use parking_lot::Mutex;
use winston_transport::{DynQueryHandle, Transport};

use crate::{
    logger_options::OverflowPolicy,
    pipeline::{make_writer_builder, TransportWriterBuilder, DEFAULT_TRANSPORT_QUEUE_CAPACITY},
};

/// Configuration the Logger holds about a registered transport.
///
/// The write side of a transport is consumed when the fanout task admits it —
/// at that point the typed transport is wrapped in a `TransportSink` and moved
/// into a `WritableStream`. The read side (`query_handle`) is
/// extracted before consumption and stored separately so `Logger::query`
/// keeps working independently of the writer's lifetime.
#[derive(Clone)]
pub struct LoggerTransport {
    /// One-shot. The fanout takes this when admitting the transport;
    /// subsequent reads see `None`.
    builder: Arc<Mutex<Option<TransportWriterBuilder>>>,
    /// Long-lived. Open as many query streams as you like, even after the
    /// writer side has been consumed.
    query_handle: Option<Arc<dyn DynQueryHandle>>,
    level: Option<String>,
    format: Option<Arc<FormatPipeline>>,
    overflow_policy: OverflowPolicy,
    queue_capacity: usize,
    /// Advanced override for the WritableStream high-water mark. `None` uses
    /// the cache-fit default (`min(queue_capacity, 64)`).
    ws_high_water_mark: Option<usize>,
}

impl LoggerTransport {
    pub fn new<T>(transport: T) -> Self
    where
        T: Transport,
    {
        // Extract the read-side handle *before* the transport gets sealed
        // into the writer builder closure (which moves it).
        let query_handle: Option<Arc<dyn DynQueryHandle>> =
            transport.query_handle().map(Arc::from);

        Self {
            builder: Arc::new(Mutex::new(Some(make_writer_builder(transport)))),
            query_handle,
            level: None,
            format: None,
            overflow_policy: OverflowPolicy::default(),
            queue_capacity: DEFAULT_TRANSPORT_QUEUE_CAPACITY,
            ws_high_water_mark: None,
        }
    }

    pub fn with_level(mut self, level: impl Into<String>) -> Self {
        self.level = Some(level.into());
        self
    }

    pub fn with_format<F>(mut self, format: F) -> Self
    where
        F: IntoFormatPipeline,
    {
        self.format = Some(Arc::new(format.into_format_pipeline()));
        self
    }

    pub fn with_overflow_policy(mut self, policy: OverflowPolicy) -> Self {
        self.overflow_policy = policy;
        self
    }

    /// Set the per-transport **mailbox** capacity — the deep buffer the caller
    /// dispatches into, which bears the slot's [`OverflowPolicy`]. This is the
    /// burst-absorption depth: a `Block` slot parks the caller, and a `Drop`
    /// slot starts dropping, once this many entries are in flight.
    ///
    /// The `WritableStream` behind the mailbox adds only a small cache-fit buffer
    /// (~64 entries by default, not a second copy of this value), so worst-case
    /// in-flight is ~`capacity + 64`, not `2 × capacity`. In steady state both
    /// sit near-empty. That WS depth is an absolute cache-fit constant, not a
    /// function of this capacity, and is itself tunable for advanced cases via
    /// [`with_ws_high_water_mark`](Self::with_ws_high_water_mark) — see ADR 0007
    /// and `docs/queue-depth-investigation.md`.
    ///
    /// Clamped to a minimum of 1. Defaults to
    /// [`DEFAULT_TRANSPORT_QUEUE_CAPACITY`].
    pub fn with_queue_capacity(mut self, capacity: usize) -> Self {
        self.queue_capacity = capacity.max(1);
        self
    }

    /// Override the WritableStream high-water mark for this transport.
    ///
    /// **Advanced.** Most transports should leave this at the default. It is
    /// *not* a buffer size in the usual sense — increasing it generally makes
    /// sustained throughput **worse**, not better, and raises memory. If that
    /// sounds backwards, read on before using it.
    ///
    /// # What it controls
    ///
    /// The depth of the internal queue between the per-slot pump and the
    /// `WritableStream` controller — the pump→sink throttle. It carries **no**
    /// overflow policy (that is [`with_queue_capacity`](Self::with_queue_capacity)
    /// + [`OverflowPolicy`]); it only sets how far the pump may run ahead of the
    /// sink before it parks.
    ///
    /// # Why the default is shallow, and why bigger is usually worse
    ///
    /// In a saturated pipeline the cross-thread handoff cost is hidden, so a
    /// shallow queue wins on **cache locality**: the controller reads entries the
    /// pump just wrote, still in cache; a deep queue makes it read cold, evicted
    /// data — slower — and raises worst-case in-flight memory. The
    /// sustained-throughput optimum is therefore an absolute count,
    /// `≈ L1_cache_size / sizeof(FormattedEntry)` (~64 by default), independent
    /// of `queue_capacity`. See ADR 0007 and `docs/queue-depth-investigation.md`.
    ///
    /// # When overriding is justified
    ///
    /// - **A batching sink** — one whose `write` drains several entries per call
    ///   (a buffered file, an HTTP sink that POSTs N entries) — wants a *deeper*
    ///   WS so the controller has a batch ready. Set it near your batch size.
    ///   This is the one case where deeper genuinely helps.
    /// - **Unusual hardware or very large entries** shift the cache-fit optimum.
    ///   To find yours, adapt `winston/benches/queue_depth.rs` with your own
    ///   transport, or estimate `L1 / sizeof(FormattedEntry)`.
    ///
    /// Unlike the default, an explicit value is **not** capped to
    /// `queue_capacity` — set it deliberately. Clamped to a minimum of 1.
    pub fn with_ws_high_water_mark(mut self, hwm: usize) -> Self {
        self.ws_high_water_mark = Some(hwm.max(1));
        self
    }

    pub fn get_level(&self) -> Option<&String> {
        self.level.as_ref()
    }

    pub fn get_format(&self) -> Option<Arc<FormatPipeline>> {
        self.format.clone()
    }

    pub fn overflow_policy(&self) -> OverflowPolicy {
        self.overflow_policy
    }

    pub fn queue_capacity(&self) -> usize {
        self.queue_capacity
    }

    /// Explicit WritableStream high-water mark override, if set via
    /// [`with_ws_high_water_mark`](Self::with_ws_high_water_mark). `None` means
    /// the cache-fit default applies.
    pub fn ws_high_water_mark(&self) -> Option<usize> {
        self.ws_high_water_mark
    }

    pub fn query_handle(&self) -> Option<&Arc<dyn DynQueryHandle>> {
        self.query_handle.as_ref()
    }

    /// Take the one-shot writer builder. Returns `None` if already consumed.
    /// Only the fanout task calls this.
    pub(crate) fn take_builder(&self) -> Option<TransportWriterBuilder> {
        self.builder.lock().take()
    }
}

impl fmt::Debug for LoggerTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoggerTransport")
            .field("level", &self.level)
            .field("format", &self.format.as_ref().map(|_| "Format<...>"))
            .field("queryable", &self.query_handle.is_some())
            .field("overflow_policy", &self.overflow_policy)
            .field("queue_capacity", &self.queue_capacity)
            .field("ws_high_water_mark", &self.ws_high_water_mark)
            .finish()
    }
}

pub trait IntoLoggerTransport {
    fn into_logger_transport(self) -> LoggerTransport;
}

impl<T> IntoLoggerTransport for T
where
    T: Transport,
{
    fn into_logger_transport(self) -> LoggerTransport {
        LoggerTransport::new(self)
    }
}

impl IntoLoggerTransport for LoggerTransport {
    fn into_logger_transport(self) -> LoggerTransport {
        self
    }
}
