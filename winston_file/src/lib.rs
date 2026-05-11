use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use dateparser::parse;
use logform::LogInfo;
use serde_json::Value;
use whatwg_streams::{
    ReadableSource, ReadableStreamDefaultController, StreamResult, WritableSink,
    WritableStreamDefaultController,
};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use parking_lot::Mutex;
use whatwg_streams::{
    CountQueuingStrategy, DefaultStream, ReadableStream, StreamError, Unlocked,
};
use winston_transport::{
    BoxedReadableSource, DynIngestHandle, DynQueryHandle, DynReadableSource, LogQuery, Transport,
};

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
/// The writer lives behind an `Arc<Mutex<...>>` so a [`FileRotateHandle`]
/// extracted via [`FileTransport::rotate_handle`] can perform out-of-band
/// rotation (rename + reopen) concurrently with the live writer.
///
/// The lock is uncontended on the hot path — the `WritableStream` task is
/// the only writer in normal operation; rotation requests are rare. Held
/// only during sync I/O (`writeln!`, `flush`, `rename`), never across an
/// `await`.
pub struct FileTransport {
    inner: Arc<Mutex<FileInner>>,
    path: PathBuf,
}

struct FileInner {
    writer: BufWriter<File>,
}

impl FileTransport {
    pub fn new(options: FileTransportOptions) -> std::io::Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&options.filename)?;
        Ok(Self {
            inner: Arc::new(Mutex::new(FileInner {
                writer: BufWriter::new(file),
            })),
            path: options.filename,
        })
    }

    pub fn builder() -> FileTransportBuilder {
        FileTransportBuilder::new()
    }

    /// Extract a handle for out-of-band file rotation. Pull this off before
    /// handing the transport to a Logger; the handle keeps working after the
    /// transport is consumed by the `WritableStream`.
    ///
    /// See [`FileRotateHandle::rotate_and_drain`] for the destructive
    /// proxy pattern (atomic rename → drain into another sink → delete).
    pub fn rotate_handle(&self) -> FileRotateHandle {
        FileRotateHandle {
            inner: Arc::clone(&self.inner),
            path: self.path.clone(),
        }
    }
}

impl WritableSink<LogInfo> for FileTransport {
    async fn write(
        &mut self,
        info: LogInfo,
        _controller: &mut WritableStreamDefaultController,
    ) -> StreamResult<()> {
        let mut inner = self.inner.lock();
        writeln!(&mut inner.writer, "{}", info)?;
        Ok(())
    }

    async fn close(self) -> StreamResult<()> {
        let mut inner = self.inner.lock();
        inner.writer.flush()?;
        Ok(())
    }
}

impl Transport for FileTransport {
    fn query_handle(&self) -> Option<Box<dyn DynQueryHandle>> {
        Some(Box::new(FileQueryHandle {
            path: self.path.clone(),
        }))
    }

    fn ingest_handle(&self) -> Option<Box<dyn DynIngestHandle>> {
        Some(Box::new(FileIngestHandle {
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

/// Handle for atomically rotating the live file out of the way and reading
/// its contents.
///
/// The intended flow is the destructive-proxy pattern: rotate, drain the
/// renamed file into another sink (HTTP, MongoDB, …) via its `ingest_handle`,
/// then delete. See [`Self::rotate_and_drain`].
pub struct FileRotateHandle {
    inner: Arc<Mutex<FileInner>>,
    path: PathBuf,
}

impl FileRotateHandle {
    /// Atomically rename the active file out of the way and open the renamed
    /// (now-frozen) file as a `ReadableStream<LogInfo>`. The live writer
    /// continues uninterrupted at the original path — its internal file
    /// handle is swapped to a fresh file at the same path inside the same
    /// critical section as the rename, so no entries straddle the boundary.
    ///
    /// `spawn_fn` is the task spawner used to drive the returned stream's
    /// internal task — same shape as the one you pass to a Logger.
    ///
    /// Returns a [`FileDrain`] holding the renamed path and the stream.
    /// Consume the stream, then call [`FileDrain::cleanup`] to delete the
    /// renamed file. (Skip the cleanup to keep the rotated file on disk.)
    ///
    /// # Failure modes
    ///
    /// - The rename or the reopen could fail (disk full, permissions, …).
    ///   In that case the live writer is unchanged — no data lost.
    /// - The renamed file could conflict with an existing file. The renamed
    ///   path includes a timestamp + counter to make this practically
    ///   impossible.
    pub fn rotate_and_drain<F, R>(
        &self,
        spawn_fn: F,
    ) -> std::io::Result<FileDrain>
    where
        F: FnOnce(Pin<Box<dyn Future<Output = ()> + Send + 'static>>) -> R,
    {
        let renamed_path = {
            let mut inner = self.inner.lock();
            // Flush before rename so the renamed file gets everything that's
            // been logged up to this point.
            inner.writer.flush()?;
            let renamed = unique_renamed_path(&self.path)?;
            std::fs::rename(&self.path, &renamed)?;
            // Reopen at the original path so the live writer keeps going.
            // If this fails, the original path is now empty and the live
            // writer's still-open handle refers to the renamed inode — roll
            // the rename back so the path and the handle agree again.
            let new_file = match OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
            {
                Ok(f) => f,
                Err(open_err) => {
                    if let Err(rollback_err) = std::fs::rename(&renamed, &self.path) {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::Other,
                            format!(
                                "rotate_and_drain: reopen of {} failed ({open_err}) and \
                                 rollback rename {} -> {} also failed ({rollback_err}); \
                                 the live writer is now appending to {}, which is no longer \
                                 at the configured path",
                                self.path.display(),
                                renamed.display(),
                                self.path.display(),
                                renamed.display(),
                            ),
                        ));
                    }
                    return Err(open_err);
                }
            };
            inner.writer = BufWriter::new(new_file);
            renamed
        };

        let source = FileSource::open(&renamed_path, LogQuery::new())?;
        let source: Box<dyn DynReadableSource> = Box::new(source);
        let stream = ReadableStream::builder(BoxedReadableSource(source))
            .strategy(CountQueuingStrategy::new(64))
            .spawn(spawn_fn);

        Ok(FileDrain {
            path: renamed_path,
            stream,
        })
    }
}

/// Outcome of [`FileRotateHandle::rotate_and_drain`]. Holds the renamed
/// (frozen) file path and an opened stream over its contents. Typical usage:
/// split into parts, drain the stream, then `remove_file(&path)`.
pub struct FileDrain {
    path: PathBuf,
    stream: ReadableStream<LogInfo, BoxedReadableSource, DefaultStream, Unlocked>,
}

impl FileDrain {
    /// Path of the renamed file on disk.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Split into the renamed path and the stream. Use this when you want to
    /// drain the stream and then delete the file yourself (the common case).
    pub fn into_parts(
        self,
    ) -> (
        PathBuf,
        ReadableStream<LogInfo, BoxedReadableSource, DefaultStream, Unlocked>,
    ) {
        (self.path, self.stream)
    }

    /// Discard the (possibly unread) stream and delete the renamed file in
    /// one shot. Convenience for "I just want to drop the rotated logs."
    pub fn discard(self) -> std::io::Result<()> {
        drop(self.stream);
        std::fs::remove_file(&self.path)
    }
}

/// Produce a never-before-seen path adjacent to `path`. Uses a microsecond
/// timestamp + a small counter so concurrent calls don't collide.
fn unique_renamed_path(path: &Path) -> std::io::Result<PathBuf> {
    let stem = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("log");
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let ts = chrono::Utc::now().format("%Y%m%d%H%M%S%f").to_string();
    for counter in 0u32..1000 {
        let candidate = parent.join(format!("{stem}.drain-{ts}-{counter}"));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not find a unique renamed path after 1000 attempts",
    ))
}

/// Out-of-band ingest target. Each call opens a fresh append-mode file
/// handle, writes every entry as a line, flushes, and closes. Safe to run
/// concurrently with the live `WritableSink` writer because POSIX `O_APPEND`
/// guarantees atomic appends per write call (within `PIPE_BUF`).
struct FileIngestHandle {
    path: PathBuf,
}

impl DynIngestHandle for FileIngestHandle {
    fn ingest<'s>(
        &'s self,
        logs: Vec<LogInfo>,
    ) -> Pin<Box<dyn Future<Output = whatwg_streams::StreamResult<()>> + Send + 's>> {
        Box::pin(async move {
            if logs.is_empty() {
                return Ok(());
            }
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
                .map_err(StreamError::other)?;
            let mut writer = BufWriter::new(file);
            for entry in logs {
                writeln!(&mut writer, "{}", entry).map_err(StreamError::other)?;
            }
            writer.flush().map_err(StreamError::other)?;
            Ok(())
        })
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
pub struct FileSource {
    reader: Option<BufReader<File>>,
    query: LogQuery,
    /// Matching entries seen so far — drives `query.start` (skip first N).
    visited: usize,
    /// Entries enqueued so far — drives `query.limit`.
    emitted: usize,
}

impl FileSource {
    /// Open `path` as a streaming source filtered by `query`. Used directly
    /// by callers that need to read JSON-lines-formatted log files outside
    /// the `FileTransport` lifecycle — e.g. shipping rotated files into a
    /// remote target.
    pub fn open(path: impl AsRef<Path>, query: LogQuery) -> std::io::Result<Self> {
        let file = File::open(path.as_ref())?;
        Ok(Self {
            reader: Some(BufReader::new(file)),
            query,
            visited: 0,
            emitted: 0,
        })
    }
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

    /// End-to-end destructive proxy: write some entries, rotate, drain the
    /// renamed file, delete it, then keep writing — the new entries land in
    /// a fresh active file at the original path with no stale data carried
    /// over.
    #[test]
    fn rotate_and_drain_freezes_and_reopens_atomically() {
        let path = unique_path("rotate");
        let _ = std::fs::remove_file(&path);

        // Build the transport + grab the rotate handle BEFORE the
        // WritableStream consumes it. (Same pattern as query/ingest handles.)
        let transport = FileTransport::builder().filename(&path).build();
        let rotate = transport.rotate_handle();

        let stream = WritableStream::builder(transport)
            .strategy(CountQueuingStrategy::new(8))
            .spawn(thread_spawner);
        let (_locked, writer) = stream.get_writer().expect("get_writer");

        // Phase 1: write two entries, then rotate.
        futures::executor::block_on(async {
            writer.write(json_log("info", "before-rotate-1")).await.unwrap();
            writer.write(json_log("info", "before-rotate-2")).await.unwrap();
        });

        let drain = rotate
            .rotate_and_drain(thread_spawner)
            .expect("rotate_and_drain");
        let (renamed_path, drain_stream) = drain.into_parts();

        // Read everything from the renamed file's stream.
        let drained: Vec<LogInfo> = futures::executor::block_on(async {
            let (_locked_r, reader) = drain_stream.get_reader().expect("get_reader");
            let mut out = Vec::new();
            while let Some(entry) = reader.read().await.expect("read") {
                out.push(entry);
            }
            out
        });
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].message, "before-rotate-1");
        assert_eq!(drained[1].message, "before-rotate-2");

        // Delete the renamed file once we're done with its contents.
        assert!(renamed_path.exists(), "renamed file should exist on disk");
        std::fs::remove_file(&renamed_path).expect("remove renamed file");
        assert!(!renamed_path.exists(), "renamed file should be deleted");

        // Phase 2: live writer keeps going at the original path.
        futures::executor::block_on(async {
            writer.write(json_log("warn", "after-rotate")).await.unwrap();
            writer.close().await.unwrap();
        });

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(
            after.contains("after-rotate"),
            "active file should contain post-rotate entries: {after:?}"
        );
        assert!(
            !after.contains("before-rotate"),
            "active file should NOT contain pre-rotate entries (those were drained)"
        );

        let _ = std::fs::remove_file(&path);
    }

    /// Verifies `ingest_handle`: extract a handle, ingest a batch, confirm
    /// the entries land in the file. Mirrors the legacy `Proxy::ingest`
    /// use case (out-of-band batch acceptance).
    #[test]
    fn ingest_handle_appends_batch_to_file() {
        let path = unique_path("ingest");
        let _ = std::fs::remove_file(&path);

        let transport = FileTransport::builder().filename(&path).build();
        let handle = transport.ingest_handle().expect("ingest_handle is Some");

        futures::executor::block_on(async {
            handle
                .ingest(vec![
                    json_log("info", "first"),
                    json_log("warn", "second"),
                ])
                .await
                .expect("ingest");
        });
        drop(transport);

        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("first"));
        assert!(contents.contains("second"));

        let _ = std::fs::remove_file(&path);
    }
}
