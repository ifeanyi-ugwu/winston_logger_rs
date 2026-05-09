//! Prototype: file transport as WHATWG `WritableSink` + `ReadableSource`.
//!
//! `FileSink` is the analogue of [`crate::FileTransport`]'s write side.
//! `FileSource` is the streaming analogue of `FileTransport::query` — emits
//! matching log entries one at a time as a `ReadableStream<LogInfo>`.
//!
//! Lives alongside the legacy `FileTransport` so we can compare ergonomics and
//! behavior side-by-side before committing to a crate-wide migration.

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
use winston_transport::LogQuery;

// ── WritableSink ────────────────────────────────────────────────────────────

pub struct FileSink {
    writer: BufWriter<File>,
}

impl FileSink {
    pub fn new(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path.as_ref())?;
        Ok(Self {
            writer: BufWriter::new(file),
        })
    }
}

impl WritableSink<LogInfo> for FileSink {
    // The WritableStream serializes calls, so we hold the writer by &mut self.
    // FileTransport had to wrap the BufWriter in a Mutex for the legacy
    // `Transport::log(&self)` contract; that's gone here.
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

// ── ReadableSource ──────────────────────────────────────────────────────────

/// Streaming counterpart of `FileTransport::query`. Reads the file line-by-line
/// in `pull`, enqueueing matching entries one at a time.
///
/// Caveat — sort order: `LogQuery::order` is intentionally ignored at the
/// source level. A streaming source emits in file order (which is ascending
/// timestamp for typical append-only logs); honoring `Order::Descending`
/// requires reading the whole file first, which defeats streaming. Consumers
/// that need a specific order should collect the stream and sort, or pipe
/// through a sorting transform.
pub struct FileSource {
    reader: Option<BufReader<File>>,
    query: LogQuery,
    /// Total lines visited so far — drives `query.start` (skip N matching
    /// entries before emitting). Mirrors the legacy `index >= start` check.
    visited: usize,
    /// Entries enqueued so far — drives `query.limit`.
    emitted: usize,
}

impl FileSource {
    pub fn new(path: impl AsRef<Path>, query: LogQuery) -> std::io::Result<Self> {
        let file = File::open(path.as_ref())?;
        Ok(Self {
            reader: Some(BufReader::new(file)),
            query,
            visited: 0,
            emitted: 0,
        })
    }

    /// Convenience for callers that just want a path: returns the source ready
    /// to be wrapped in a `ReadableStream`.
    pub fn from_path(path: PathBuf, query: LogQuery) -> std::io::Result<Self> {
        Self::new(&path, query)
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

        // Pull semantic: read forward until we either enqueue exactly one
        // matching entry, hit EOF, or hit the limit. Backpressure (when to
        // call us again) is the stream's concern.
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
                    // EOF
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

            // Honor `start`: skip the first N matching entries.
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

// ── Helpers (currently duplicated with FileTransport — collapse on full migration) ──

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
        Value::String(ts_str) => parse(ts_str).ok().map(|dt| dt.with_timezone(&Utc)),
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

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn unique_path(stem: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "winston_file_streams_{stem}_{}_{n}.log",
            std::process::id()
        ))
    }

    fn json_log(level: &str, msg: &str) -> LogInfo {
        let log = LogInfo::new(level, msg);
        let log = timestamp().transform(log).unwrap();
        json().transform(log).unwrap()
    }

    fn thread_spawner() -> impl FnOnce(
        std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
    ) -> std::thread::JoinHandle<()>
           + Clone {
        |fut: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>| {
            std::thread::spawn(move || futures::executor::block_on(fut))
        }
    }

    #[test]
    fn file_sink_writes_through_writable_stream() {
        let path = unique_path("sink_basic");
        let _ = std::fs::remove_file(&path);

        let sink = FileSink::new(&path).expect("open file");
        let stream = WritableStream::builder(sink)
            .strategy(CountQueuingStrategy::new(8))
            .spawn(thread_spawner());

        futures::executor::block_on(async {
            let (_locked, writer) = stream.get_writer().expect("get_writer");
            writer.write(json_log("info", "alpha")).await.unwrap();
            writer.write(json_log("warn", "beta")).await.unwrap();
            writer.close().await.unwrap();
        });

        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("alpha"), "missing alpha: {contents:?}");
        assert!(contents.contains("beta"), "missing beta: {contents:?}");

        let _ = std::fs::remove_file(&path);
    }

    /// Round-trip: write three entries via FileSink, then query them back as a
    /// stream via FileSource. Filters out one by level. Demonstrates both halves
    /// of the new transport contract working together.
    #[test]
    fn file_source_streams_query_results_with_level_filter() {
        let path = unique_path("source_filter");
        let _ = std::fs::remove_file(&path);

        // Write three entries.
        {
            let sink = FileSink::new(&path).expect("open for write");
            let stream = WritableStream::builder(sink)
                .strategy(CountQueuingStrategy::new(8))
                .spawn(thread_spawner());

            futures::executor::block_on(async {
                let (_locked, writer) = stream.get_writer().expect("get_writer");
                writer.write(json_log("info", "first")).await.unwrap();
                writer.write(json_log("error", "second")).await.unwrap();
                writer.write(json_log("info", "third")).await.unwrap();
                writer.close().await.unwrap();
            });
        }

        // Query: only `info` level.
        let mut query = LogQuery::new();
        query.levels = vec!["info".to_string()];
        let source = FileSource::new(&path, query).expect("open for read");
        let read_stream = ReadableStream::builder(source)
            .strategy(CountQueuingStrategy::new(8))
            .spawn(thread_spawner());

        let collected: Vec<LogInfo> = futures::executor::block_on(async {
            let (_locked, reader) = read_stream.get_reader().expect("get_reader");
            let mut out = Vec::new();
            while let Some(entry) = reader.read().await.expect("read") {
                out.push(entry);
            }
            out
        });

        assert_eq!(collected.len(), 2, "expected 2 info entries, got {collected:?}");
        assert_eq!(collected[0].message, "first");
        assert_eq!(collected[1].message, "third");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn file_source_honors_limit_and_closes_stream() {
        let path = unique_path("source_limit");
        let _ = std::fs::remove_file(&path);

        {
            let sink = FileSink::new(&path).expect("open for write");
            let stream = WritableStream::builder(sink)
                .strategy(CountQueuingStrategy::new(8))
                .spawn(thread_spawner());

            futures::executor::block_on(async {
                let (_locked, writer) = stream.get_writer().expect("get_writer");
                for i in 0..5 {
                    writer.write(json_log("info", &format!("msg{i}"))).await.unwrap();
                }
                writer.close().await.unwrap();
            });
        }

        let mut query = LogQuery::new();
        query.limit = Some(2);
        let source = FileSource::new(&path, query).expect("open for read");
        let read_stream = ReadableStream::builder(source)
            .strategy(CountQueuingStrategy::new(8))
            .spawn(thread_spawner());

        let collected: Vec<LogInfo> = futures::executor::block_on(async {
            let (_locked, reader) = read_stream.get_reader().expect("get_reader");
            let mut out = Vec::new();
            while let Some(entry) = reader.read().await.expect("read") {
                out.push(entry);
            }
            out
        });

        assert_eq!(collected.len(), 2, "limit should cap at 2");

        let _ = std::fs::remove_file(&path);
    }
}
