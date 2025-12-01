pub mod align;
pub mod cli;
pub mod colorize;
mod format;
pub mod json;
pub mod label;
pub mod logstash;
mod macros;
pub mod metadata;
pub mod ms;
pub mod pad_levels;
pub mod pretty_print;
pub mod printf;
pub mod simple;
pub mod timestamp;
pub mod uncolorize;
pub use format::Format;
pub mod passthrough;
/* chaining of formats can be achieved by the `.chain` method on the `Format`
instance hence the `combine` format is not needed  */
