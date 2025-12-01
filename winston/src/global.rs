use crate::{logger::TransportHandle, Logger};
use logform::LogInfo;
use std::sync::OnceLock;
use winston_transport::Transport;

static GLOBAL_LOGGER: OnceLock<Logger> = OnceLock::new();

/// Initialize the global logger. Must be called once before using other functions.
///
/// # Panics
/// Panics if called more than once.
///
/// # Example
/// ```rust
/// use winston::{transports::stdout, Logger};
/// use std::sync::Arc;
///
/// // Create and configure logger
/// let mut logger = Logger::new(None);
/// logger.add_transport(stdout());
///
/// // Make it global
/// winston::init(logger);
/// ```
pub fn init(logger: Logger) {
    GLOBAL_LOGGER
        .set(logger)
        .expect("Global logger already initialized. Call init() only once.");
}

/// Get the global logger instance.
///
/// # Panics
/// Panics if `init()` hasn't been called yet.
fn global_logger() -> &'static Logger {
    GLOBAL_LOGGER
        .get()
        .expect("Global logger not initialized. Call winston::global::init() first.")
}

/// Try to get the global logger instance without panicking.
/// Returns None if not initialized.
fn try_global_logger() -> Option<&'static Logger> {
    GLOBAL_LOGGER.get()
}

/// Check if the global logger has been initialized.
pub fn is_initialized() -> bool {
    GLOBAL_LOGGER.get().is_some()
}

pub fn log(entry: logform::LogInfo) {
    global_logger().log(entry);
}

/// Try to log without panicking if not initialized.
/// Returns false if logger not initialized.
pub fn try_log(entry: logform::LogInfo) -> bool {
    if let Some(logger) = try_global_logger() {
        logger.log(entry);
        true
    } else {
        false
    }
}

pub fn configure(new_options: Option<crate::LoggerOptions>) {
    global_logger().configure(new_options);
}

pub fn flush() -> Result<(), String> {
    global_logger().flush()
}

pub fn close() {
    global_logger().close();
}

pub fn query(options: &winston_transport::LogQuery) -> Result<Vec<logform::LogInfo>, String> {
    global_logger().query(options)
}

/// Add a transport to the global logger and return a handle for later removal.
pub fn add_transport<T>(transport: T) -> TransportHandle
where
    T: Transport<LogInfo> + Send + Sync + 'static,
{
    global_logger().add_transport(transport)
}

/// Remove a transport by its handle.
/// Returns `true` if the transport was found and removed, `false` otherwise.
pub fn remove_transport(handle: TransportHandle) -> bool {
    global_logger().remove_transport(handle)
}

/// Register the global logger with the `log` crate.
/// Must be called after `init()`.
#[cfg(feature = "log-backend")]
pub fn register_with_log() -> Result<(), log::SetLoggerError> {
    let logger = global_logger();
    log::set_logger(logger)?;
    log::set_max_level(log::LevelFilter::Trace);
    Ok(())
}
