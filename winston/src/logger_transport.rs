use std::{fmt, sync::Arc};

use logform::{Format, IntoFormatPipeline, LogInfo};
use parking_lot::Mutex;
use winston_transport::{DynQueryHandle, Transport};

use crate::{
    logger_options::OverflowPolicy,
    pipeline::{make_writer_builder, TransportWriterBuilder, DEFAULT_TRANSPORT_QUEUE_CAPACITY},
};

/// Configuration the Logger holds about a registered transport.
///
/// The write side of a `Transport: WritableSink<LogInfo>` is consumed when
/// the fanout task admits the transport — at that point the typed transport
/// is moved into a `WritableStream`. The read side (`query_handle`) is
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
    format: Option<Arc<dyn Format<Input = LogInfo> + Send + Sync>>,
    overflow_policy: OverflowPolicy,
    queue_capacity: usize,
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

    /// Set the per-transport queue capacity.
    ///
    /// The value is applied to *both* layers between fanout and sink: the
    /// mailbox the fanout dispatches into, and the WritableStream's
    /// high-water mark the pump enqueues against. Total in-flight chunks
    /// for this transport can therefore reach up to ~2× this value before
    /// the slot's [`OverflowPolicy`] kicks in.
    ///
    /// Clamped to a minimum of 1. Defaults to
    /// [`DEFAULT_TRANSPORT_QUEUE_CAPACITY`].
    pub fn with_queue_capacity(mut self, capacity: usize) -> Self {
        self.queue_capacity = capacity.max(1);
        self
    }

    pub fn get_level(&self) -> Option<&String> {
        self.level.as_ref()
    }

    pub fn get_format(&self) -> Option<Arc<dyn Format<Input = LogInfo> + Send + Sync>> {
        self.format.clone()
    }

    pub fn overflow_policy(&self) -> OverflowPolicy {
        self.overflow_policy
    }

    pub fn queue_capacity(&self) -> usize {
        self.queue_capacity
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
