mod global;
mod log_macros;
mod logger;
mod logger_builder;
mod logger_levels;
mod logger_options;
mod logger_transport;
pub mod transports;

#[cfg(feature = "log-backend")]
pub use global::register_with_log;
pub use global::{
    add_transport, close, configure, flush, init, is_initialized, log, query, remove_transport,
    try_log,
};
pub use logform as format;
pub use logger::Logger;
pub use logger_options::{BackpressureStrategy, LoggerOptions};
pub use logger_transport::LoggerTransport;
pub use winston_transport::LogQuery;
