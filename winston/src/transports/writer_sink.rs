//! Prototype: `WriterSink` is the analogue of
//! [`winston_transport::transport_adapters::WriterTransport`] but implements
//! [`whatwg_streams::WritableSink`] instead of the legacy `Transport` trait.
//!
//! Lives alongside the existing transport so we can compare ergonomics and
//! behavior side-by-side before committing to a crate-wide migration.

use std::{
    fmt::Display,
    io::{self, Write},
    marker::PhantomData,
};

use whatwg_streams::{StreamResult, WritableSink, WritableStreamDefaultController};

pub struct WriterSink<W, L>
where
    W: Write + Send + 'static,
    L: Display + Send + 'static,
{
    writer: W,
    _phantom: PhantomData<fn() -> L>,
}

impl<W, L> WriterSink<W, L>
where
    W: Write + Send + 'static,
    L: Display + Send + 'static,
{
    pub fn new(writer: W) -> Self {
        Self {
            writer,
            _phantom: PhantomData,
        }
    }
}

impl<W, L> WritableSink<L> for WriterSink<W, L>
where
    W: Write + Send + 'static,
    L: Display + Send + 'static,
{
    // The WritableStream serializes calls into this sink, which is why the
    // signature can be `&mut self` instead of `&self`. WriterTransport had to
    // wrap the writer in a Mutex to satisfy the legacy `Transport::log(&self)`
    // contract; here that's gone.
    async fn write(
        &mut self,
        chunk: L,
        _controller: &mut WritableStreamDefaultController,
    ) -> StreamResult<()> {
        writeln!(&mut self.writer, "{}", chunk)?;
        Ok(())
    }

    // `close` is the WHATWG signal that no more writes are coming. We flush
    // here so callers of `WritableStream::close` get the same durability
    // guarantee that `Transport::flush` provides today.
    async fn close(mut self) -> StreamResult<()> {
        self.writer.flush()?;
        Ok(())
    }
}

pub fn stdout_sink<L>() -> WriterSink<io::Stdout, L>
where
    L: Display + Send + 'static,
{
    WriterSink::new(io::stdout())
}

pub fn stderr_sink<L>() -> WriterSink<io::Stderr, L>
where
    L: Display + Send + 'static,
{
    WriterSink::new(io::stderr())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use whatwg_streams::{CountQueuingStrategy, WritableStream};

    /// `Vec<u8>` writer that's cheap to clone-by-handle for assertions —
    /// matches the role `TestBuffer` plays in the legacy adapter tests.
    #[derive(Clone)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl SharedBuf {
        fn new() -> Self {
            Self(Arc::new(Mutex::new(Vec::new())))
        }
        fn contents(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    impl Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Drives a `WriterSink<SharedBuf, String>` through a real `WritableStream`,
    /// proving the end-to-end path: writer.write → sink.write → underlying buf.
    #[test]
    fn writer_sink_writes_chunks_through_writable_stream() {
        let buf = SharedBuf::new();
        let sink: WriterSink<SharedBuf, String> = WriterSink::new(buf.clone());

        let stream = WritableStream::builder(sink)
            .strategy(CountQueuingStrategy::new(8))
            .spawn(|fut| {
                std::thread::spawn(move || futures::executor::block_on(fut));
            });

        futures::executor::block_on(async {
            let (_locked, writer) = stream.get_writer().expect("get_writer");
            writer.write("first".to_string()).await.expect("write 1");
            writer.write("second".to_string()).await.expect("write 2");
            writer.close().await.expect("close");
        });

        let out = buf.contents();
        assert!(out.contains("first"), "missing first line: {out:?}");
        assert!(out.contains("second"), "missing second line: {out:?}");
    }
}
