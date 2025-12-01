use std::io;
pub use winston_file::FileTransport as File;
pub use winston_transport::transport_adapters::WriterTransport;
pub use winston_transport::*;

// Convenience functions
pub fn stdout() -> WriterTransport<io::Stdout, LogInfo> {
    WriterTransport::new(io::stdout())
}

pub fn stderr() -> WriterTransport<io::Stderr, LogInfo> {
    WriterTransport::new(io::stderr())
}
