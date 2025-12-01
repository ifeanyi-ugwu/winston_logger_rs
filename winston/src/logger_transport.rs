use std::{fmt, sync::Arc};

use logform::{Format, LogInfo};
use winston_transport::Transport;

#[derive(Clone)]
pub struct LoggerTransport<L> {
    transport: Arc<dyn Transport<L> + Send + Sync>,
    level: Option<String>,
    format: Option<Arc<dyn Format<Input = L> + Send + Sync>>,
}

impl<L> LoggerTransport<L> {
    pub fn new<T>(transport: T) -> Self
    where
        T: Transport<L> + Send + Sync + 'static,
    {
        Self {
            transport: Arc::new(transport),
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

    pub fn get_transport(&self) -> &Arc<dyn Transport<L> + Send + Sync> {
        &self.transport
    }
}

impl<L> fmt::Debug for LoggerTransport<L> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoggerTransport")
            .field(
                "transport",
                &format!("Transport<{}>", std::any::type_name::<L>()),
            )
            .field("level", &self.level)
            .field("format", &self.format.as_ref().map(|_| "Format<...>"))
            .finish()
    }
}

pub trait IntoLoggerTransport {
    fn into_logger_transport(self) -> LoggerTransport<LogInfo>;
}

// Raw transport
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
