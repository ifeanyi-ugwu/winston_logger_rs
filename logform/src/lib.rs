pub mod config;
mod formats;
mod log_info;
mod utils;

pub use formats::{
    align::align, cli::cli, colorize::colorize, json::json, label::label, logstash::logstash,
    metadata::metadata, ms::ms, pad_levels::pad_levels, passthrough::passthrough,
    pretty_print::pretty_print, printf::printf, simple::simple, timestamp::timestamp,
    uncolorize::uncolorize, Format,
};
pub use log_info::LogInfo;
