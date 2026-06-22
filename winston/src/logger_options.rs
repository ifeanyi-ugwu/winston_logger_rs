use crate::{
    logger::TransportHandle,
    logger_levels::LoggerLevels,
    logger_transport::{IntoLoggerTransport, LoggerTransport},
};
use logform::{json, Format, IntoFormatPipeline, LogInfo};
use std::{collections::HashMap, sync::Arc};

#[derive(Clone)]
pub struct LoggerOptions {
    pub levels: Option<LoggerLevels>,
    pub format: Option<Arc<dyn Format<Input = LogInfo> + Send + Sync>>,
    pub level: Option<String>,
    pub transports: Option<Vec<(TransportHandle, LoggerTransport)>>,
}

impl LoggerOptions {
    /// Creates a new `LoggerOptions` instance with default settings.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the logging level for the logger.
    ///
    /// # Arguments
    ///
    /// * `level` - A string slice that represents the logging level.
    pub fn level<T: Into<String>>(mut self, level: T) -> Self {
        self.level = Some(level.into());
        self
    }

    /// Sets the log format for the logger.
    ///
    /// # Arguments
    ///
    /// * `format` - The log format to be used.
    pub fn format<F>(mut self, format: F) -> Self
    where
        F: IntoFormatPipeline,
    {
        self.format = Some(Arc::new(format.into_format_pipeline()));
        self
    }

    /// Adds a single transport to the existing list of transports.
    ///
    /// This method is **additive** — it appends the provided transport to any
    /// previously added transports. Each transport is automatically wrapped in
    /// an [`Arc`] and assigned a unique [`TransportHandle`].
    ///
    /// This method accepts either a raw transport or a pre-configured
    /// [`LoggerTransport`].
    ///
    /// # Example
    /// ```ignore
    /// use winston_rs::LoggerOptions;
    ///
    /// // Raw transport
    /// let options = LoggerOptions::new()
    ///     .transport(stdout())
    ///     .transport(FileTransport::new("app.log"));
    ///
    /// // Pre-configured transport
    /// let options = LoggerOptions::new()
    ///     .transport(
    ///         LoggerTransport::new(FileTransport::new("app.log"))
    ///             .with_level("debug")
    ///             .with_format(json())
    ///     );
    /// ```
    ///
    /// Each call to [`transport`](Self::transport) appends a new transport,
    /// allowing multiple outputs (e.g. console + file + network) to be used simultaneously.
    pub fn transport(mut self, transport: impl IntoLoggerTransport) -> Self {
        self.transports
            .get_or_insert_with(Vec::new)
            .push((TransportHandle::new(), transport.into_logger_transport()));
        self
    }

    /// Replaces all transports with the provided collection.
    ///
    /// This method is **not additive** — it replaces any previously configured
    /// transports. Each transport must already be wrapped in an [`Arc`] and will
    /// be assigned a unique [`TransportHandle`].
    ///
    /// # Example
    /// ```ignore
    /// use winston_rs::LoggerOptions;
    /// use std::sync::Arc;
    ///
    /// let transports = vec![
    ///     Arc::new(stdout()),
    ///     Arc::new(FileTransport::new("app.log")),
    /// ];
    /// let options = LoggerOptions::new().transports(transports);
    /// ```
    ///
    /// Use this method when you want to **replace** all existing transports
    /// instead of appending new ones. Multiple calls to `.transports()` will
    /// override the previous collection.
    pub fn transports<I>(mut self, transports: I) -> Self
    where
        I: IntoIterator,
        I::Item: IntoLoggerTransport,
    {
        self.transports = Some(
            transports
                .into_iter()
                .map(|t| (TransportHandle::new(), t.into_logger_transport()))
                .collect(),
        );
        self
    }

    /// Sets custom logging levels for the logger.
    ///
    /// # Arguments
    ///
    /// * `levels` - A `HashMap` where the key is the level name and the value is its severity.
    pub fn levels(mut self, levels: HashMap<String, u8>) -> Self {
        self.levels = Some(LoggerLevels::new(levels));
        self
    }

}

impl Default for LoggerOptions {
    /// Default: info-level filter, empty transport list, JSON format,
    /// standard level table. There is no caller-side backpressure knob:
    /// pressure is configured per transport via [`OverflowPolicy`] and
    /// [`LoggerTransport::with_queue_capacity`]
    /// (see `docs/adr/0002-direct-dispatch-backpressure.md`).
    fn default() -> Self {
        LoggerOptions {
            levels: Some(LoggerLevels::default()),
            level: Some("info".to_string()),
            transports: Some(Vec::new()),
            format: Some(Arc::new(json().into_format_pipeline())),
        }
    }
}

impl std::fmt::Debug for LoggerOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoggerOptions")
            .field("levels", &self.levels)
            .field("level", &self.level)
            .field("transports", &self.transports)
            .field("format", &"<Format trait object>")
            .finish()
    }
}

/// Per-transport policy applied when the slot's mailbox is full.
///
/// `Block` propagates pressure end-to-end: a saturated `Block` slot parks
/// the calling thread on the slot's mailbox `Condvar` until room appears.
/// Pick this for durability sinks (file, daily-rotate) where dropping is
/// unacceptable. Under sustained pressure a `Block` slot will slow the
/// producer to the sink's rate — that's the design ("slowest pipe sets
/// the pace"); pick a Drop policy on a transport where you don't want
/// that to happen.
///
/// `DropNewest` short-circuits at the mailbox boundary: the new entry is
/// dropped, the slot never parks the caller. Pick this for telemetry
/// lanes (HTTP, Mongo, console) where freshness matters more than
/// completeness.
///
/// `DropOldest` evicts the head of the mailbox and pushes the new entry —
/// a sliding window of the most recent N entries. Pick this when freshness
/// matters more than completeness *and* the most recent entries are the
/// useful ones (metrics, dashboards, last-known-state telemetry).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OverflowPolicy {
    Block,
    DropNewest,
    DropOldest,
}

impl Default for OverflowPolicy {
    fn default() -> Self {
        OverflowPolicy::Block
    }
}
