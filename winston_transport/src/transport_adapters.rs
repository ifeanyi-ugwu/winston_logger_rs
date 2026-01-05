//! This module contains adapter implementations for converting between
//! `Transport` and `Write` traits, both for owned and borrowed scenarios.
//!
//! The adapters allow seamless interoperability:
//! - `TransportWriter` - use any Transport as a Writer
//! - `WriterTransport` - use any Writer as a Transport
//! - Both owned and borrowed variants are provided
//!
//! Extension traits provide convenient `.into_writer()`, `.as_writer()`,
//! `.into_transport()`, and `.as_transport()` methods.
//!
//! All adapters are completely generic over the log type `L`.

use crate::Transport;
use std::{
    cell::RefCell,
    fmt::Display,
    io::{self, Write},
    sync::Mutex,
};

/// A trait for creating log entries from strings.
/// This allows the adapter to work with any log type.
pub trait FromString {
    fn from_string(s: String) -> Self;
}

/// owned adapter: takes ownership of a Transport and uses it as a Writer.
/// Generic over any log type `L` that implements `FromString`.
pub struct TransportWriter<T, L>
where
    T: Transport<L>,
    L: FromString,
{
    transport: T,
    buffer: Vec<u8>,
    _phantom: std::marker::PhantomData<L>,
}

impl<T, L> TransportWriter<T, L>
where
    T: Transport<L>,
    L: FromString,
{
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            buffer: Vec::new(),
            _phantom: std::marker::PhantomData,
        }
    }
}

impl<T, L> Write for TransportWriter<T, L>
where
    T: Transport<L>,
    L: FromString,
{
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buffer.extend_from_slice(buf);
        // Process full lines
        while let Some(pos) = self.buffer.iter().position(|&b| b == b'\n') {
            let line_bytes = self.buffer.drain(..=pos).collect::<Vec<u8>>();
            let line = String::from_utf8_lossy(&line_bytes)
                .trim_end_matches(&['\r', '\n'][..])
                .to_string();

            let log_entry = L::from_string(line);
            self.transport.log(log_entry);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        // Flush any buffered partial line before calling underlying flush
        if !self.buffer.is_empty() {
            let leftover = String::from_utf8_lossy(&self.buffer).to_string();
            self.buffer.clear();
            let log_entry = L::from_string(leftover);
            self.transport.log(log_entry);
        }

        self.transport.flush().map_err(|e| io::Error::other(e))
    }
}

impl<T, L> Drop for TransportWriter<T, L>
where
    T: Transport<L>,
    L: FromString,
{
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

/// borrowed adapter: borrows a Transport and uses it as a Writer.
/// Generic over any log type `L` that implements `FromString`.
pub struct TransportWriterRef<'a, T, L>
where
    T: Transport<L> + ?Sized,
    L: FromString,
{
    transport: &'a T,
    buffer: RefCell<Vec<u8>>,
    _phantom: std::marker::PhantomData<L>,
}

impl<'a, T, L> TransportWriterRef<'a, T, L>
where
    T: Transport<L> + ?Sized,
    L: FromString,
{
    pub fn new(transport: &'a T) -> Self {
        Self {
            transport,
            buffer: RefCell::new(Vec::new()),
            _phantom: std::marker::PhantomData,
        }
    }

    // Helper to flush internal buffer emitting logs for each full line
    fn flush_buffered_lines(&self) {
        let mut buf = self.buffer.borrow_mut();
        let mut start = 0;

        while let Some(pos) = buf[start..].iter().position(|&b| b == b'\n') {
            let end = start + pos;
            // Extract the line + '\n'
            let line_bytes = buf.drain(..=end).collect::<Vec<_>>();
            let line_str = String::from_utf8_lossy(&line_bytes)
                .trim_end_matches(&['\r', '\n'][..])
                .to_string();
            let log_entry = L::from_string(line_str);
            self.transport.log(log_entry);
            start = 0; // dropped above
        }
    }
}

impl<'a, T, L> Write for TransportWriterRef<'a, T, L>
where
    T: Transport<L> + ?Sized,
    L: FromString,
{
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // Append to internal buffer
        {
            let mut internal_buffer = self.buffer.borrow_mut();
            internal_buffer.extend_from_slice(buf);
        }

        // Process any full lines in the buffer
        self.flush_buffered_lines();

        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        // Flush any remaining partial line
        let leftover = self.buffer.borrow_mut().drain(..).collect::<Vec<u8>>();
        if !leftover.is_empty() {
            let leftover_str = String::from_utf8_lossy(&leftover).to_string();
            let log_entry = L::from_string(leftover_str);
            self.transport.log(log_entry);
        }

        self.transport.flush().map_err(|e| io::Error::other(e))
    }
}

impl<'a, T, L> Drop for TransportWriterRef<'a, T, L>
where
    T: Transport<L> + ?Sized,
    L: FromString,
{
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

/// owned adapter to use a Writer as a Transport.
/// Generic over any log type `L` that implements `Display`.
pub struct WriterTransport<W, L>
where
    W: Write,
    L: Display,
{
    pub writer: Mutex<W>,
    _phantom: std::marker::PhantomData<L>,
}

impl<W, L> WriterTransport<W, L>
where
    W: Write,
    L: Display,
{
    pub fn new(writer: W) -> Self {
        Self {
            writer: Mutex::new(writer),
            _phantom: std::marker::PhantomData,
        }
    }
}

impl<W, L> Transport<L> for WriterTransport<W, L>
where
    W: Write,
    L: Display,
{
    fn log(&self, info: L) {
        if let Ok(mut writer) = self.writer.lock() {
            let _ = writeln!(writer, "{}", info);
        }
    }

    fn log_batch(&self, infos: Vec<L>) {
        if infos.is_empty() {
            return;
        }
        if let Ok(mut writer) = self.writer.lock() {
            for info in infos {
                if let Err(e) = writeln!(writer, "{}", info) {
                    eprintln!(
                        "Failed to write log entry in batch to WriterTransport: {}",
                        e
                    );
                }
            }
        } else {
            eprintln!("Failed to acquire writer lock for WriterTransport batch logging");
        }
    }

    fn flush(&self) -> Result<(), String> {
        self.writer
            .lock()
            .map_err(|_| "Failed to lock writer".to_string())
            .and_then(|mut writer| {
                writer
                    .flush()
                    .map_err(|e| format!("Failed to flush: {}", e))
            })
    }
}

impl<W, L> Drop for WriterTransport<W, L>
where
    W: Write,
    L: Display,
{
    fn drop(&mut self) {
        if let Ok(mut writer) = self.writer.lock() {
            let _ = writer.flush();
        }
    }
}

/// borrowed adapter for using a Writer as a Transport.
/// Generic over any log type `L` that implements `Display`.
pub struct WriterTransportRef<'a, W, L>
where
    W: Write,
    L: Display,
{
    writer: &'a Mutex<W>,
    _phantom: std::marker::PhantomData<L>,
}

impl<'a, W, L> WriterTransportRef<'a, W, L>
where
    W: Write,
    L: Display,
{
    pub fn new(writer: &'a Mutex<W>) -> Self {
        Self {
            writer,
            _phantom: std::marker::PhantomData,
        }
    }
}

impl<'a, W, L> Transport<L> for WriterTransportRef<'a, W, L>
where
    W: Write,
    L: Display,
{
    fn log(&self, info: L) {
        if let Ok(mut writer) = self.writer.lock() {
            let _ = writeln!(writer, "{}", info);
        }
    }

    fn log_batch(&self, infos: Vec<L>) {
        if infos.is_empty() {
            return;
        }

        if let Ok(mut writer) = self.writer.lock() {
            for info in infos {
                if let Err(e) = writeln!(writer, "{}", info) {
                    eprintln!(
                        "Failed to write log entry in batch to WriterTransportRef: {}",
                        e
                    );
                }
            }
        } else {
            eprintln!("Failed to acquire writer lock for WriterTransportRef batch logging");
        }
    }

    fn flush(&self) -> Result<(), String> {
        self.writer
            .lock()
            .map_err(|_| "Failed to lock writer".to_string())
            .and_then(|mut writer| {
                writer
                    .flush()
                    .map_err(|e| format!("Failed to flush: {}", e))
            })
    }
}

impl<'a, W, L> Drop for WriterTransportRef<'a, W, L>
where
    W: Write,
    L: Display,
{
    fn drop(&mut self) {
        if let Ok(mut writer) = self.writer.lock() {
            let _ = writer.flush();
        }
    }
}

/// extension trait for converting an owned transport to a writer.
pub trait IntoTransportWriter<L>: Transport<L> + Sized
where
    L: FromString,
{
    fn into_writer(self) -> TransportWriter<Self, L> {
        TransportWriter::new(self)
    }
}

/// extension trait for converting a borrowed transport to a writer.
pub trait AsTransportWriter<L>: Transport<L>
where
    L: FromString,
{
    fn as_writer(&self) -> TransportWriterRef<'_, Self, L> {
        TransportWriterRef::new(self)
    }
}

impl<T, L> IntoTransportWriter<L> for T
where
    T: Transport<L>,
    L: FromString,
{
}

impl<T, L> AsTransportWriter<L> for T
where
    T: Transport<L>,
    L: FromString,
{
}

/// trait to convert an owned writer into a transport.
pub trait IntoWriterTransport<L>: Write + Sized
where
    L: Display,
{
    fn into_transport(self) -> WriterTransport<Self, L> {
        WriterTransport::new(self)
    }
}

impl<W, L> IntoWriterTransport<L> for W
where
    W: Write,
    L: Display,
{
}

/// extension trait for converting a borrowed writer to a transport.
pub trait AsWriterTransport<L>
where
    L: Display,
{
    type Writer: Write;
    fn as_transport(&self) -> WriterTransportRef<'_, Self::Writer, L>;
}

impl<W, L> AsWriterTransport<L> for Mutex<W>
where
    W: Write,
    L: Display,
{
    type Writer = W;

    fn as_transport(&self) -> WriterTransportRef<'_, W, L> {
        WriterTransportRef::new(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    // Example log type for testing
    #[derive(Clone, Debug, PartialEq)]
    struct TestLog {
        message: String,
    }

    impl FromString for TestLog {
        fn from_string(s: String) -> Self {
            Self { message: s }
        }
    }

    impl Display for TestLog {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}", self.message)
        }
    }

    #[derive(Clone)]
    struct MockTransport {
        messages: Arc<Mutex<Vec<TestLog>>>,
    }

    impl MockTransport {
        fn new() -> Self {
            Self {
                messages: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn get_messages(&self) -> Vec<TestLog> {
            self.messages.lock().unwrap().clone()
        }
    }

    impl Transport<TestLog> for MockTransport {
        fn log(&self, info: TestLog) {
            self.messages.lock().unwrap().push(info);
        }
    }

    struct TestBuffer {
        content: Vec<u8>,
    }

    impl TestBuffer {
        fn new() -> Self {
            Self {
                content: Vec::new(),
            }
        }

        fn contents_as_string(&self) -> String {
            String::from_utf8_lossy(&self.content).to_string()
        }
    }

    impl Write for TestBuffer {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.content.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn test_owned_transport_to_writer() {
        let transport = MockTransport::new();
        let transport_clone = transport.clone();
        let mut writer: TransportWriter<_, TestLog> = transport.into_writer();

        writeln!(writer, "Test message 1").unwrap();
        writeln!(writer, "Test message 2").unwrap();

        let messages = transport_clone.get_messages();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].message, "Test message 1");
        assert_eq!(messages[1].message, "Test message 2");
    }

    #[test]
    fn test_borrowed_transport_to_writer() {
        let transport = MockTransport::new();
        let mut writer: TransportWriterRef<'_, _, TestLog> = transport.as_writer();

        writeln!(writer, "Borrowed message 1").unwrap();
        writeln!(writer, "Borrowed message 2").unwrap();

        let messages = transport.get_messages();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].message, "Borrowed message 1");
        assert_eq!(messages[1].message, "Borrowed message 2");
    }

    #[test]
    fn test_owned_writer_to_transport() {
        let buffer = TestBuffer::new();
        let transport: WriterTransport<_, TestLog> = buffer.into_transport();

        transport.log(TestLog::from_string("Test log 1".to_string()));
        transport.log(TestLog::from_string("Test log 2".to_string()));

        let writer_guard = transport.writer.lock().unwrap();
        let content = writer_guard.contents_as_string();
        assert!(content.contains("Test log 1"));
        assert!(content.contains("Test log 2"));
    }

    #[test]
    fn test_borrowed_writer_to_transport() {
        let test_buffer = Mutex::new(TestBuffer::new());
        let transport_ref: WriterTransportRef<'_, _, TestLog> = test_buffer.as_transport();

        transport_ref.log(TestLog::from_string("Borrowed log 1".to_string()));
        transport_ref.log(TestLog::from_string("Borrowed log 2".to_string()));
        transport_ref.flush().unwrap();

        let buffer_guard = test_buffer.lock().unwrap();
        let contents = buffer_guard.contents_as_string();
        assert!(contents.contains("Borrowed log 1"));
        assert!(contents.contains("Borrowed log 2"));
    }
}
