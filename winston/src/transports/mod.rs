use std::{
    fmt::Display,
    io::{self, Write},
    marker::PhantomData,
};

use whatwg_streams::{StreamResult, WritableSink, WritableStreamDefaultController};

pub use winston_file::FileTransport as File;
pub use winston_transport::*;

/// Generic transport that forwards each log entry to an `io::Write` writer
/// via `Display`. The `WritableStream` the Logger spawns serializes calls,
/// so no `Mutex` is needed around the writer.
pub struct WriterTransport<W, L>
where
    W: Write + Send + Sync + 'static,
    L: Display + Send + Sync + 'static,
{
    writer: W,
    _phantom: PhantomData<fn() -> L>,
}

impl<W, L> WriterTransport<W, L>
where
    W: Write + Send + Sync + 'static,
    L: Display + Send + Sync + 'static,
{
    pub fn new(writer: W) -> Self {
        Self {
            writer,
            _phantom: PhantomData,
        }
    }
}

impl<W, L> WritableSink<L> for WriterTransport<W, L>
where
    W: Write + Send + Sync + 'static,
    L: Display + Send + Sync + 'static,
{
    async fn write(
        &mut self,
        chunk: L,
        _controller: &mut WritableStreamDefaultController,
    ) -> StreamResult<()> {
        writeln!(&mut self.writer, "{}", chunk)?;
        Ok(())
    }

    async fn close(mut self) -> StreamResult<()> {
        self.writer.flush()?;
        Ok(())
    }
}

impl<W> Transport for WriterTransport<W, FormattedEntry>
where
    W: Write + Send + Sync + 'static,
{
    // Stdout/stderr/etc. don't keep history — query is unsupported.
}

/// Convenience: `WriterTransport` that writes to standard output.
pub fn stdout() -> WriterTransport<io::Stdout, FormattedEntry> {
    WriterTransport::new(io::stdout())
}

/// Convenience: `WriterTransport` that writes to standard error.
pub fn stderr() -> WriterTransport<io::Stderr, FormattedEntry> {
    WriterTransport::new(io::stderr())
}
