use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use dateparser::parse;
use logform::LogInfo;
use serde_json::Value;
use whatwg_streams::{
    ReadableSource, ReadableStreamDefaultController, StreamResult, WritableSink,
    WritableStreamDefaultController,
};
use winston_transport::{DynQueryHandle, DynReadableSource, LogQuery, Transport};

// ── Options / builder ────────────────────────────────────────────────────────

pub struct FileTransportOptions {
    pub filename: PathBuf,
}

pub struct FileTransportBuilder {
    filename: Option<PathBuf>,
}

impl Default for FileTransportBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl FileTransportBuilder {
    pub fn new() -> Self {
        Self { filename: None }
    }

    pub fn filename<T: Into<PathBuf>>(mut self, filename: T) -> Self {
        self.filename = Some(filename.into());
        self
    }

    pub fn build(self) -> FileTransport {
        FileTransport::new(FileTransportOptions {
            filename: self.filename.expect("filename is required"),
        })
        .expect("failed to open log file")
    }
}

// ── FileTransport ────────────────────────────────────────────────────────────

/// File-backed transport.
///
/// Writes are serialized by the `WritableStream` the Logger spawns over this
/// sink, so no `Mutex` is needed around the buffered writer. Query opens a
/// fresh read-only handle for each call and streams matching entries via
/// `FileSource`.
pub struct FileTransport {
    writer: BufWriter<File>,
    path: PathBuf,
}

impl FileTransport {
    pub fn new(options: FileTransportOptions) -> std::io::Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&options.filename)?;
        Ok(Self {
            writer: BufWriter::new(file),
            path: options.filename,
        })
    }

    pub fn builder() -> FileTransportBuilder {
        FileTransportBuilder::new()
    }
}

impl WritableSink<LogInfo> for FileTransport {
    async fn write(
        &mut self,
        info: LogInfo,
        _controller: &mut WritableStreamDefaultController,
    ) -> StreamResult<()> {
        writeln!(&mut self.writer, "{}", info)?;
        Ok(())
    }

    async fn close(mut self) -> StreamResult<()> {
        self.writer.flush()?;
        Ok(())
    }
}

impl Transport for FileTransport {
    fn query_handle(&self) -> Option<Box<dyn DynQueryHandle>> {
        Some(Box::new(FileQueryHandle {
            path: self.path.clone(),
        }))
    }
}

/// Read-only handle the Logger keeps after the transport's writer half has
/// been moved into a `WritableStream`. Holds the file path and opens a fresh
/// `FileSource` per query.
struct FileQueryHandle {
    path: PathBuf,
}

impl DynQueryHandle for FileQueryHandle {
    fn query(&self, options: &LogQuery) -> Option<Box<dyn DynReadableSource>> {
        let file = File::open(&self.path).ok()?;
        Some(Box::new(FileSource {
            reader: Some(BufReader::new(file)),
            query: options.clone(),
            visited: 0,
            emitted: 0,
        }))
    }
}

// ── FileSource (streaming query) ─────────────────────────────────────────────

/// Reads the underlying file line-by-line, enqueuing entries that match the
/// query one at a time. Backpressure is handled by the `ReadableStream` the
/// Logger wraps this in.
///
/// Note on ordering: `LogQuery::order` is intentionally ignored. A streaming
/// source emits in file order (which is ascending timestamp for typical
/// append-only logs); honoring `Order::Descending` would require reading the
/// whole file first, which defeats streaming. Consumers that need a specific
/// order should collect the stream and sort.
struct FileSource {
    reader: Option<BufReader<File>>,
    query: LogQuery,
    /// Matching entries seen so far — drives `query.start` (skip first N).
    visited: usize,
    /// Entries enqueued so far — drives `query.limit`.
    emitted: usize,
}

impl ReadableSource<LogInfo> for FileSource {
    async fn pull(
        &mut self,
        controller: &mut ReadableStreamDefaultController<LogInfo>,
    ) -> StreamResult<()> {
        let Some(reader) = self.reader.as_mut() else {
            return Ok(());
        };

        let limit = self.query.limit.unwrap_or(usize::MAX);
        let start = self.query.start.unwrap_or(0);

        // Read forward until we either enqueue exactly one matching entry, hit
        // EOF, or hit the limit.
        let mut line = String::new();
        loop {
            if self.emitted >= limit {
                let _ = controller.close();
                self.reader = None;
                return Ok(());
            }

            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => {
                    let _ = controller.close();
                    self.reader = None;
                    return Ok(());
                }
                Err(e) => return Err(e.into()),
                Ok(_) => {}
            }

            let trimmed = line.trim_end_matches(&['\r', '\n'][..]);
            let Some(entry) = parse_log_entry(trimmed) else {
                continue;
            };
            if !matches_query(&self.query, &entry) {
                continue;
            }

            self.visited += 1;
            if self.visited <= start {
                continue;
            }

            let projected = if self.query.fields.is_empty() {
                entry
            } else {
                project_fields(entry, &self.query.fields)
            };

            let _ = controller.enqueue(projected);
            self.emitted += 1;
            return Ok(());
        }
    }
}

// ── Parsing / matching helpers ──────────────────────────────────────────────

fn parse_log_entry(line: &str) -> Option<LogInfo> {
    let parsed: Value = serde_json::from_str(line).ok()?;
    let level = parsed["level"].as_str()?;
    let message = parsed["message"].as_str()?;
    let meta = parsed
        .as_object()?
        .iter()
        .filter_map(|(k, v)| {
            if k != "level" && k != "message" {
                Some((k.clone(), v.clone()))
            } else {
                None
            }
        })
        .collect::<HashMap<_, _>>();
    Some(LogInfo::from_parts(level, message, meta))
}

fn extract_timestamp(entry: &LogInfo) -> Option<DateTime<Utc>> {
    entry.meta.get("timestamp").and_then(|value| match value {
        Value::String(ts) => parse(ts).ok().map(|dt| dt.with_timezone(&Utc)),
        _ => None,
    })
}

fn matches_query(query: &LogQuery, entry: &LogInfo) -> bool {
    if !query.levels.is_empty() && !query.levels.contains(&entry.level) {
        return false;
    }
    if let Some(from) = query.from {
        match extract_timestamp(entry) {
            Some(ts) if ts >= from => {}
            _ => return false,
        }
    }
    if let Some(until) = query.until {
        match extract_timestamp(entry) {
            Some(ts) if ts <= until => {}
            _ => return false,
        }
    }
    if let Some(ref regex) = query.search_term
        && !regex.is_match(&entry.message)
    {
        return false;
    }
    if let Some(ref filter) = query.filter
        && !filter.evaluate(&entry.to_flat_value())
    {
        return false;
    }
    true
}

fn project_fields(entry: LogInfo, fields: &[String]) -> LogInfo {
    let normalized: Vec<String> = fields.iter().map(|f| f.to_lowercase()).collect();
    LogInfo::from_parts(
        if normalized.iter().any(|f| f == "level") {
            entry.level
        } else {
            String::new()
        },
        if normalized.iter().any(|f| f == "message") {
            entry.message
        } else {
            String::new()
        },
        entry
            .meta
            .into_iter()
            .filter(|(k, _)| normalized.iter().any(|f| f == &k.to_lowercase()))
            .collect(),
    )
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use logform::{json, timestamp, Format};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use whatwg_streams::{CountQueuingStrategy, ReadableStream, WritableStream};
    use winston_transport::BoxedReadableSource;

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn unique_path(stem: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "winston_file_{stem}_{}_{n}.log",
            std::process::id()
        ))
    }

    fn json_log(level: &str, msg: &str) -> LogInfo {
        let log = LogInfo::new(level, msg);
        let log = timestamp().transform(log).unwrap();
        json().transform(log).unwrap()
    }

    fn thread_spawner<F>(fut: F) -> std::thread::JoinHandle<()>
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        std::thread::spawn(move || futures::executor::block_on(fut))
    }

    #[test]
    fn writes_through_writable_stream() {
        let path = unique_path("write");
        let _ = std::fs::remove_file(&path);

        let transport = FileTransport::builder().filename(&path).build();
        let stream = WritableStream::builder(transport)
            .strategy(CountQueuingStrategy::new(8))
            .spawn(thread_spawner);

        futures::executor::block_on(async {
            let (_locked, writer) = stream.get_writer().expect("get_writer");
            writer.write(json_log("info", "alpha")).await.unwrap();
            writer.write(json_log("warn", "beta")).await.unwrap();
            writer.close().await.unwrap();
        });

        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("alpha"));
        assert!(contents.contains("beta"));

        let _ = std::fs::remove_file(&path);
    }

    /// Round-trip: write three entries, query them back via `Transport::query`
    /// → `BoxedReadableSource` → `ReadableStream`. Filter to `info` only.
    #[test]
    fn query_streams_results_with_level_filter() {
        let path = unique_path("query");
        let _ = std::fs::remove_file(&path);

        // Write
        {
            let transport = FileTransport::builder().filename(&path).build();
            let stream = WritableStream::builder(transport)
                .strategy(CountQueuingStrategy::new(8))
                .spawn(thread_spawner);

            futures::executor::block_on(async {
                let (_locked, writer) = stream.get_writer().expect("get_writer");
                writer.write(json_log("info", "first")).await.unwrap();
                writer.write(json_log("error", "second")).await.unwrap();
                writer.write(json_log("info", "third")).await.unwrap();
                writer.close().await.unwrap();
            });
        }

        // Query — extract the handle from a fresh transport. (In normal use
        // the Logger does this at registration time, before the transport is
        // consumed by its WritableStream.)
        let read_transport = FileTransport::builder().filename(&path).build();
        let handle = read_transport
            .query_handle()
            .expect("query_handle returned None");
        let mut q = LogQuery::new();
        q.levels = vec!["info".to_string()];
        let source = handle.query(&q).expect("query returned None");

        let read_stream = ReadableStream::builder(BoxedReadableSource(source))
            .strategy(CountQueuingStrategy::new(8))
            .spawn(thread_spawner);

        let collected: Vec<LogInfo> = futures::executor::block_on(async {
            let (_locked, reader) = read_stream.get_reader().expect("get_reader");
            let mut out = Vec::new();
            while let Some(entry) = reader.read().await.expect("read") {
                out.push(entry);
            }
            out
        });

        assert_eq!(collected.len(), 2);
        assert_eq!(collected[0].message, "first");
        assert_eq!(collected[1].message, "third");

        let _ = std::fs::remove_file(&path);
    }
}
