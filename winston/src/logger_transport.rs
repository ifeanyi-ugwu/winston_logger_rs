use std::{fmt, future::Future, pin::Pin, sync::Arc};

use futures::channel::mpsc as fmpsc;
use logform::{Format, LogInfo};
use parking_lot::Mutex;
use winston_transport::{DynQueryHandle, Transport};

use crate::pipeline::{run_transport_task, SpawnFn, TransportMessage};

/// One-shot builder that captures a typed transport and, when invoked by the
/// pipeline, produces the future for that transport's per-transport task.
///
/// The transport is type-erased into the closure so a heterogeneous list of
/// `LoggerTransport`s can live in the FanoutSink. Mutex<Option<_>> gives
/// "callable exactly once" — the slot empties when the pipeline takes it.
type TaskBuilder = Box<
    dyn FnOnce(
            fmpsc::UnboundedReceiver<TransportMessage>,
            Option<Arc<dyn Format<Input = LogInfo> + Send + Sync>>, // transport-level
            Option<Arc<dyn Format<Input = LogInfo> + Send + Sync>>, // global
            SpawnFn,
        ) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>
        + Send,
>;

/// Configuration the Logger holds about a registered transport.
///
/// The write side of a `Transport: WritableSink<LogInfo>` is consumed when
/// the pipeline spawns the per-transport task — at that point the typed
/// transport is moved into a `WritableStream`. The read side (`query_handle`)
/// is extracted before consumption and stored separately so `Logger::query`
/// keeps working independently of the writer's lifetime.
#[derive(Clone)]
pub struct LoggerTransport {
    /// One-shot. The pipeline takes this when spawning; subsequent reads see `None`.
    builder: Arc<Mutex<Option<TaskBuilder>>>,
    /// Long-lived. Open as many query streams as you like, even after the
    /// writer side has been consumed.
    query_handle: Option<Arc<dyn DynQueryHandle>>,
    level: Option<String>,
    format: Option<Arc<dyn Format<Input = LogInfo> + Send + Sync>>,
}

impl LoggerTransport {
    pub fn new<T>(transport: T) -> Self
    where
        T: Transport,
    {
        // Extract the read-side handle *before* the transport gets sealed
        // into the task builder closure (which moves it).
        let query_handle: Option<Arc<dyn DynQueryHandle>> =
            transport.query_handle().map(Arc::from);

        let builder: TaskBuilder = Box::new(
            move |rx, transport_format, global_format, spawn_fn| {
                Box::pin(run_transport_task(
                    rx,
                    transport,
                    transport_format,
                    global_format,
                    spawn_fn,
                ))
            },
        );

        Self {
            builder: Arc::new(Mutex::new(Some(builder))),
            query_handle,
            level: None,
            format: None,
        }
    }

    pub fn with_level(mut self, level: impl Into<String>) -> Self {
        self.level = Some(level.into());
        self
    }

    pub fn with_format<F>(mut self, format: F) -> Self
    where
        F: Format<Input = LogInfo> + Send + Sync + 'static,
    {
        self.format = Some(Arc::new(format));
        self
    }

    pub fn get_level(&self) -> Option<&String> {
        self.level.as_ref()
    }

    pub fn get_format(&self) -> Option<Arc<dyn Format<Input = LogInfo> + Send + Sync>> {
        self.format.clone()
    }

    pub fn query_handle(&self) -> Option<&Arc<dyn DynQueryHandle>> {
        self.query_handle.as_ref()
    }

    /// Take the one-shot task builder. Returns `None` if already consumed.
    /// Only the pipeline calls this.
    pub(crate) fn take_builder(&self) -> Option<TaskBuilder> {
        self.builder.lock().take()
    }
}

impl fmt::Debug for LoggerTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoggerTransport")
            .field("level", &self.level)
            .field("format", &self.format.as_ref().map(|_| "Format<...>"))
            .field("queryable", &self.query_handle.is_some())
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
