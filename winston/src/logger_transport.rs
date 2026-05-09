use std::{fmt, sync::Arc};

use logform::{Format, LogInfo};
use winston_transport::{AsyncTransport, Transport};

/// Whether a `LoggerTransport` wraps a synchronous or async transport.
///
/// The pipeline matches on this in `run_transport_task` so each leaf method is
/// called the right way: sync transports are fire-and-forget, async transports
/// are `await`ed inside the per-transport task.
#[derive(Clone)]
pub enum TransportKind<L> {
    Sync(Arc<dyn Transport<L> + Send + Sync>),
    Async(Arc<dyn AsyncTransport<L> + Send + Sync>),
}

#[derive(Clone)]
pub struct LoggerTransport<L> {
    kind: TransportKind<L>,
    level: Option<String>,
    format: Option<Arc<dyn Format<Input = L> + Send + Sync>>,
}

impl<L> LoggerTransport<L> {
    pub fn new<T>(transport: T) -> Self
    where
        T: Transport<L> + Send + Sync + 'static,
    {
        Self {
            kind: TransportKind::Sync(Arc::new(transport)),
            level: None,
            format: None,
        }
    }

    pub fn new_async<T>(transport: T) -> Self
    where
        T: AsyncTransport<L> + 'static,
    {
        Self {
            kind: TransportKind::Async(Arc::new(transport)),
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
        F: Format<Input = L> + Send + Sync + 'static,
    {
        self.format = Some(Arc::new(format));
        self
    }

    pub fn get_level(&self) -> Option<&String> {
        self.level.as_ref()
    }

    pub fn get_format(&self) -> Option<Arc<dyn Format<Input = L> + Send + Sync>> {
        self.format.clone()
    }

    pub fn kind(&self) -> &TransportKind<L> {
        &self.kind
    }

    /// Returns the underlying sync transport, if this `LoggerTransport` wraps one.
    /// Async transports return `None` — caller must dispatch separately.
    pub fn as_sync(&self) -> Option<&Arc<dyn Transport<L> + Send + Sync>> {
        match &self.kind {
            TransportKind::Sync(t) => Some(t),
            TransportKind::Async(_) => None,
        }
    }
}

impl<L> fmt::Debug for LoggerTransport<L> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match &self.kind {
            TransportKind::Sync(_) => "Sync",
            TransportKind::Async(_) => "Async",
        };
        f.debug_struct("LoggerTransport")
            .field(
                "transport",
                &format!("Transport<{}>({})", std::any::type_name::<L>(), kind),
            )
            .field("level", &self.level)
            .field("format", &self.format.as_ref().map(|_| "Format<...>"))
            .finish()
    }
}

pub trait IntoLoggerTransport {
    fn into_logger_transport(self) -> LoggerTransport<LogInfo>;
}

// Raw sync transport
impl<T> IntoLoggerTransport for T
where
    T: Transport<LogInfo> + Send + Sync + 'static,
{
    fn into_logger_transport(self) -> LoggerTransport<LogInfo> {
        LoggerTransport::new(self)
    }
}

// Pre-configured LoggerTransport
impl IntoLoggerTransport for LoggerTransport<LogInfo> {
    fn into_logger_transport(self) -> LoggerTransport<LogInfo> {
        self
    }
}

/// Wrapper to feed an async transport through `IntoLoggerTransport` ergonomically:
///
/// ```ignore
/// logger.add_transport(Async(MyHttpTransport::new()));
/// ```
///
/// A blanket `impl<T: AsyncTransport> IntoLoggerTransport for T` would conflict
/// with the sync blanket impl for any type implementing both traits, so callers
/// opt in via `Async(..)` (or `LoggerTransport::new_async(..)`).
pub struct Async<T>(pub T);

impl<T> IntoLoggerTransport for Async<T>
where
    T: AsyncTransport<LogInfo> + 'static,
{
    fn into_logger_transport(self) -> LoggerTransport<LogInfo> {
        LoggerTransport::new_async(self.0)
    }
}
