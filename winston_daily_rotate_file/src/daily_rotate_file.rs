use chrono::{DateTime, Local, Utc};
use flate2::{write::GzEncoder, Compression};
use logform::{FormattedEntry, LogInfo};
use std::fs::{create_dir_all, read_dir, File, OpenOptions};
use std::future::Future;
use std::io::{BufWriter, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use whatwg_streams::{StreamError, StreamResult};
use winston_transport::{DynIngestHandle, Transport};

pub struct DailyRotateFileOptions {
    pub filename: PathBuf,
    pub date_pattern: String,
    pub max_files: Option<u32>,
    pub max_size: Option<u64>,
    pub dirname: Option<PathBuf>,
    pub zipped_archive: bool,
    pub utc: bool,
}

/// Daily-rotating file transport.
///
/// Writes one entry per `write` call. Before each write we check whether
/// rotation is due (date boundary or `max_size` exceeded) and roll over if so.
/// Optional gzip compression of the previous file and `max_files` retention
/// happen during rotation.
///
/// State is accessed via `&mut self` from the WritableStream's task — no
/// `Mutex` required, in contrast to the legacy `Transport<L>` impl which had
/// to lock-around-`&self` for every write.
pub struct DailyRotateFile {
    writer: BufWriter<File>,
    options: DailyRotateFileOptions,
    last_rotation: DateTime<Utc>,
    file_path: PathBuf,
    /// Shared with [`DailyRotateRotationHandle`]s for observing the current
    /// active file. Updated under lock during `rotate()` (rare); read by
    /// `list_rotated_files`. No contention with the write hot path.
    active_path_shared: Arc<RwLock<PathBuf>>,
}

impl DailyRotateFile {
    pub fn new(options: DailyRotateFileOptions) -> std::io::Result<Self> {
        let current_date = if options.utc {
            Utc::now()
        } else {
            Local::now().with_timezone(&Utc)
        };

        let (file, path) = Self::create_file(&options, &current_date)?;

        Ok(DailyRotateFile {
            writer: BufWriter::new(file),
            options,
            last_rotation: current_date,
            active_path_shared: Arc::new(RwLock::new(path.clone())),
            file_path: path,
        })
    }

    pub fn builder() -> DailyRotateFileBuilder {
        DailyRotateFileBuilder::new()
    }

    /// Extract a handle for observing rotation. Use it to list already-rotated
    /// (no longer active) log files for shipping/archiving — see
    /// [`DailyRotateRotationHandle::list_rotated_files`].
    ///
    /// Pull this off the transport before handing it to a Logger; once the
    /// Logger consumes the transport into its `WritableStream`, the transport
    /// instance is no longer accessible.
    pub fn rotation_handle(&self) -> DailyRotateRotationHandle {
        DailyRotateRotationHandle {
            dirname: self.options.dirname.clone(),
            filename: self.options.filename.clone(),
            active_path: Arc::clone(&self.active_path_shared),
        }
    }

    fn create_file(
        options: &DailyRotateFileOptions,
        date: &DateTime<Utc>,
    ) -> std::io::Result<(File, PathBuf)> {
        let filename =
            Self::get_filename(&options.filename, date, &options.date_pattern, options.utc);

        let log_dir = options.dirname.as_deref().unwrap_or_else(|| Path::new("."));
        let full_path = log_dir.join(&filename);

        let parent = full_path.parent().unwrap_or(log_dir);
        create_dir_all(parent)?;

        Self::create_unique_file(log_dir, &filename)
    }

    fn create_unique_file(log_dir: &Path, filename: &Path) -> std::io::Result<(File, PathBuf)> {
        let mut counter = 0;

        let base_name = filename
            .file_stem()
            .unwrap_or_else(|| std::ffi::OsStr::new("log"));
        let ext = filename.extension().and_then(|e| e.to_str()).unwrap_or("");

        loop {
            let new_filename = if counter == 0 {
                filename.to_path_buf()
            } else {
                let mut unique_filename = filename.to_path_buf();
                unique_filename.set_file_name(if ext.is_empty() {
                    format!("{}_{}", base_name.to_string_lossy(), counter)
                } else {
                    format!("{}_{}.{}", base_name.to_string_lossy(), counter, ext)
                });

                log_dir.join(unique_filename)
            };

            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&new_filename)
            {
                Ok(file) => return Ok((file, new_filename)),
                Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                    counter += 1;
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
    }

    fn get_filename(base_path: &Path, date: &DateTime<Utc>, pattern: &str, utc: bool) -> PathBuf {
        let date_str = if utc {
            date.format(pattern).to_string()
        } else {
            date.with_timezone(&Local).format(pattern).to_string()
        };

        let mut filename = base_path.to_path_buf();
        let original_filename = filename
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or("log");

        filename.set_file_name(format!("{}.{}", original_filename, date_str));
        filename
    }

    fn current_file_size(&mut self) -> u64 {
        let _ = self.writer.flush();
        self.writer
            .get_ref()
            .metadata()
            .map(|m| m.len())
            .unwrap_or(0)
    }

    fn should_rotate(&mut self, new_entry_size: usize) -> bool {
        let now = Utc::now();

        let now_str = if self.options.utc {
            now.format(&self.options.date_pattern).to_string()
        } else {
            now.with_timezone(&Local)
                .format(&self.options.date_pattern)
                .to_string()
        };

        let last_rotation_str = if self.options.utc {
            self.last_rotation
                .format(&self.options.date_pattern)
                .to_string()
        } else {
            self.last_rotation
                .with_timezone(&Local)
                .format(&self.options.date_pattern)
                .to_string()
        };

        if last_rotation_str != now_str {
            return true;
        }

        if let Some(max_size) = self.options.max_size {
            return self.current_file_size() + new_entry_size as u64 >= max_size;
        }
        false
    }

    fn rotate(&mut self) -> std::io::Result<()> {
        let now = Utc::now();
        let _ = self.writer.flush();

        let previous_file_path = self.file_path.clone();

        let (new_file, new_path) = Self::create_file(&self.options, &now)?;
        self.writer = BufWriter::new(new_file);
        self.file_path = new_path.clone();
        self.last_rotation = now;
        // Publish the new active path so any rotation handle's
        // `list_rotated_files` correctly excludes it.
        if let Ok(mut guard) = self.active_path_shared.write() {
            *guard = new_path;
        }

        if self.options.zipped_archive {
            if let Err(e) = Self::compress_file(&previous_file_path) {
                eprintln!("Failed to compress log file: {}", e);
            }
        }

        if let Some(max_files) = self.options.max_files {
            if let Err(e) = self.cleanup_old_files(max_files) {
                eprintln!("Failed to clean up old log files: {}", e);
            }
        }

        Ok(())
    }

    fn compress_file(file_path: &Path) -> std::io::Result<()> {
        let mut counter = 0;

        let base_name = file_path
            .file_stem()
            .unwrap_or_else(|| std::ffi::OsStr::new("compressed"));

        let original_ext = file_path
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("");

        loop {
            let attempt_path = if counter == 0 {
                if original_ext.is_empty() {
                    file_path.with_file_name(format!("{}.gz", base_name.to_string_lossy()))
                } else {
                    file_path.with_file_name(format!(
                        "{}.{}.gz",
                        base_name.to_string_lossy(),
                        original_ext
                    ))
                }
            } else {
                let unique_filename = if original_ext.is_empty() {
                    format!("{}_{}.gz", base_name.to_string_lossy(), counter)
                } else {
                    format!(
                        "{}.{}_{}.gz",
                        base_name.to_string_lossy(),
                        original_ext,
                        counter
                    )
                };

                file_path.with_file_name(unique_filename)
            };

            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&attempt_path)
            {
                Ok(gz_file) => {
                    let input_file = File::open(file_path)?;
                    let mut encoder = GzEncoder::new(gz_file, Compression::default());

                    std::io::copy(&mut &input_file, &mut encoder)?;
                    encoder.finish()?;

                    std::fs::remove_file(file_path)?;

                    return Ok(());
                }
                Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                    counter += 1;
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
    }

    fn cleanup_old_files(&self, max_files: u32) -> std::io::Result<()> {
        let log_dir = self
            .options
            .dirname
            .as_deref()
            .or_else(|| self.options.filename.parent())
            .unwrap_or_else(|| Path::new("."));

        let base_name = self
            .options
            .filename
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("log");

        let mut log_files: Vec<PathBuf> = Vec::new();

        for entry in read_dir(log_dir)? {
            let entry = entry?;
            let path = entry.path();

            if path.is_file() {
                let filename = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
                if filename.starts_with(&format!("{}.", base_name))
                    || filename.starts_with(&format!("{}_", base_name))
                {
                    log_files.push(path);
                }
            }
        }

        if log_files.len() <= max_files as usize {
            return Ok(());
        }

        // Newest first.
        log_files.sort_by(|a, b| {
            let a_time = a
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);

            let b_time = b
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);

            b_time.cmp(&a_time)
        });

        for old_file in log_files.iter().skip(max_files as usize) {
            if old_file == &self.file_path {
                continue;
            }

            if self.options.zipped_archive
                && old_file.extension().and_then(|e| e.to_str()) != Some("gz")
            {
                if let Err(e) = Self::compress_file(old_file) {
                    eprintln!("Failed to compress old file {}: {}", old_file.display(), e);
                }
            } else if let Err(e) = std::fs::remove_file(old_file) {
                eprintln!("Failed to remove old file {}: {}", old_file.display(), e);
            }
        }

        Ok(())
    }
}

impl Transport for DailyRotateFile {
    async fn log(&mut self, entry: FormattedEntry) -> StreamResult<()> {
        // Use the full Display so any logform finalizer (json, printf, etc.)
        // gets honored. This matches `winston_file::FileTransport` and lets
        // rotated files round-trip through `FileSource` for `ship_rotated_files`.
        let line = entry.to_string();
        let entry_size = line.len() + 1; // +1 for the trailing newline
        if self.should_rotate(entry_size) {
            self.rotate()?;
        }
        writeln!(&mut self.writer, "{}", line)?;
        Ok(())
    }

    async fn close(mut self) -> StreamResult<()> {
        self.writer.flush()?;
        Ok(())
    }

    // Query is intentionally unsupported — the rotation/archive lifecycle
    // means past entries live in N files (and possibly .gz archives) that the
    // transport doesn't track centrally. If you need query, log to a regular
    // `winston_file::FileTransport` (which keeps a single file) alongside.

    fn ingest_handle(&self) -> Option<Box<dyn DynIngestHandle>> {
        Some(Box::new(DailyRotateIngestHandle {
            active_path: Arc::clone(&self.active_path_shared),
        }))
    }
}

/// Out-of-band ingest target for a daily-rotate file. Each call opens the
/// *current* active file (read from the shared active-path slot) in append
/// mode, writes the batch via Display, flushes, closes. Doesn't drive
/// rotation — the live writer's date/size checks own that — so ingested
/// batches land in whatever file is active at the moment of the call.
///
/// Ordering between ingested entries and the live writer's entries isn't
/// guaranteed (the live writer is buffered; the ingest handle flushes
/// immediately). For an archive/consolidation flow that's fine.
struct DailyRotateIngestHandle {
    active_path: Arc<RwLock<PathBuf>>,
}

impl DynIngestHandle for DailyRotateIngestHandle {
    fn ingest<'s>(
        &'s self,
        logs: Vec<LogInfo>,
    ) -> Pin<Box<dyn Future<Output = StreamResult<()>> + Send + 's>> {
        Box::pin(async move {
            if logs.is_empty() {
                return Ok(());
            }
            let path = match self.active_path.read() {
                Ok(guard) => guard.clone(),
                Err(_) => return Err(StreamError::from("daily-rotate active path lock poisoned")),
            };
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
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

/// Long-lived handle the user keeps after the transport is consumed by the
/// Logger's `WritableStream`. Use it to enumerate already-rotated (no longer
/// active) log files for shipping/archiving.
///
/// # Recommended pattern: ship rotated logs to a remote target
///
/// ```ignore
/// # use winston_daily_rotate_file::DailyRotateFile;
/// # use winston::Logger;
/// # use winston_transport::Transport;
/// let drf = DailyRotateFile::builder()
///     .filename("/var/log/app.log")
///     .max_files(10)
///     .build()
///     .unwrap();
/// let rotation = drf.rotation_handle();   // pull this off before...
/// let logger = Logger::builder().transport(drf).build();
/// // ...the transport is moved.
///
/// // Periodically (e.g. on a timer):
/// for path in rotation.list_rotated_files()? {
///     // open `path` as a stream of LogInfo, pipe into your target
///     // (HttpTransport / MongoDBTransport / etc.) via its ingest_handle,
///     // then std::fs::remove_file(&path)?;
/// }
/// # Ok::<(), std::io::Error>(())
/// ```
pub struct DailyRotateRotationHandle {
    dirname: Option<PathBuf>,
    filename: PathBuf,
    /// Shared with the live transport — `rotate()` updates this so the
    /// listing always excludes the current active file even after rotations.
    active_path: Arc<RwLock<PathBuf>>,
}

impl DailyRotateRotationHandle {
    /// Returns paths of every rotated log file (plain or `.gz`) the transport
    /// has produced and not yet been cleaned up by `max_files`. The currently
    /// active file is excluded.
    ///
    /// Files are returned in arbitrary order — sort by mtime if you want
    /// oldest-first shipping.
    pub fn list_rotated_files(&self) -> std::io::Result<Vec<PathBuf>> {
        let log_dir = self
            .dirname
            .as_deref()
            .or_else(|| self.filename.parent())
            .unwrap_or_else(|| Path::new("."));

        let base_name = self
            .filename
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("log");

        let active = self.active_path.read().ok().map(|p| p.clone());

        let mut rotated = Vec::new();
        for entry in read_dir(log_dir)? {
            let entry = entry?;
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let filename = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            // Match the same patterns the rotation logic produces:
            // "<base>.<date>", "<base>_<n>.<date>", and their `.gz` variants.
            let matches = filename.starts_with(&format!("{}.", base_name))
                || filename.starts_with(&format!("{}_", base_name));
            if !matches {
                continue;
            }
            if active.as_ref().map(|a| a == &path).unwrap_or(false) {
                continue;
            }
            rotated.push(path);
        }
        Ok(rotated)
    }
}

pub struct DailyRotateFileBuilder {
    filename: Option<PathBuf>,
    date_pattern: String,
    max_files: Option<u32>,
    max_size: Option<u64>,
    dirname: Option<PathBuf>,
    zipped_archive: bool,
    utc: bool,
}

impl Default for DailyRotateFileBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl DailyRotateFileBuilder {
    pub fn new() -> Self {
        Self {
            filename: None,
            date_pattern: String::from("%Y-%m-%d"),
            max_files: None,
            max_size: None,
            dirname: None,
            zipped_archive: false,
            utc: false,
        }
    }

    pub fn filename<T: Into<PathBuf>>(mut self, filename: T) -> Self {
        self.filename = Some(filename.into());
        self
    }

    pub fn date_pattern<T: Into<String>>(mut self, pattern: T) -> Self {
        self.date_pattern = pattern.into();
        self
    }

    pub fn max_files(mut self, count: u32) -> Self {
        self.max_files = Some(count);
        self
    }

    pub fn max_size(mut self, size: u64) -> Self {
        self.max_size = Some(size);
        self
    }

    pub fn dirname<T: Into<PathBuf>>(mut self, dirname: T) -> Self {
        self.dirname = Some(dirname.into());
        self
    }

    pub fn zipped_archive(mut self, zipped: bool) -> Self {
        self.zipped_archive = zipped;
        self
    }

    pub fn utc(mut self, utc: bool) -> Self {
        self.utc = utc;
        self
    }

    pub fn build(self) -> Result<DailyRotateFile, String> {
        let filename = self.filename.ok_or("Filename is required")?;

        let options = DailyRotateFileOptions {
            filename,
            date_pattern: self.date_pattern,
            max_files: self.max_files,
            max_size: self.max_size,
            dirname: self.dirname,
            zipped_archive: self.zipped_archive,
            utc: self.utc,
        };

        DailyRotateFile::new(options).map_err(|e| format!("Failed to open log file: {}", e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Local;
    use std::fs;
    use tempfile::TempDir;
    use whatwg_streams::{CountQueuingStrategy, WritableStream};
    use winston_transport::TransportSink;

    fn fe(level: &str, msg: impl Into<String>) -> FormattedEntry {
        FormattedEntry::new(LogInfo::new(level, msg), None)
    }

    fn setup_temp_dir() -> TempDir {
        let project_root = std::env::current_dir().expect("Failed to get current directory");
        TempDir::new_in(&project_root).expect("Failed to create temp directory in project folder")
    }

    fn thread_spawner<F>(fut: F) -> std::thread::JoinHandle<()>
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        std::thread::spawn(move || futures::executor::block_on(fut))
    }

    /// Wraps the transport in a real WritableStream, runs the given closure
    /// against its writer, then closes the stream so rotation/flush completes
    /// before assertions.
    fn drive<F, Fut>(transport: DailyRotateFile, body: F)
    where
        F: FnOnce(
            whatwg_streams::WritableStreamDefaultWriter<
                FormattedEntry,
                TransportSink<DailyRotateFile>,
            >,
        ) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let stream = WritableStream::builder(TransportSink(transport))
            .strategy(CountQueuingStrategy::new(64))
            .spawn(thread_spawner);
        futures::executor::block_on(async {
            let (_locked, writer) = stream.get_writer().expect("get_writer");
            body(writer).await;
        });
    }

    #[test]
    fn test_basic_logging() {
        let temp_dir = setup_temp_dir();
        let log_path = temp_dir.path().join("test.log");
        let transport = DailyRotateFile::builder()
            .filename(&log_path)
            .date_pattern("%Y-%m-%d")
            .max_files(3)
            .max_size(1024)
            .build()
            .expect("Failed to create transport");

        drive(transport, |writer| async move {
            writer
                .write(fe("info", "Test message"))
                .await
                .expect("write");
            writer.close().await.expect("close");
        });

        let date_str = Local::now().format("%Y-%m-%d").to_string();
        let log_file = temp_dir.path().join(format!("test.log.{}", date_str));
        let contents = fs::read_to_string(log_file).expect("Failed to read log file");
        assert!(contents.contains("Test message"));
    }

    #[test]
    fn test_date_based_rotation() {
        let temp_dir = setup_temp_dir();
        let log_path = temp_dir.path().join("test.log");
        let transport = DailyRotateFile::builder()
            .filename(log_path)
            .date_pattern("%Y-%m-%d_%H-%M-%S")
            .build()
            .expect("Failed to create transport");

        drive(transport, |writer| async move {
            writer
                .write(fe("info", "log entry 1"))
                .await
                .expect("write");
            // Simulate date change — `should_rotate` looks at the formatted
            // current time vs. last rotation, so a 1s sleep with a per-second
            // pattern is enough to trigger.
            std::thread::sleep(std::time::Duration::from_secs(1));
            writer
                .write(fe("info", "log entry 2"))
                .await
                .expect("write");
            writer.close().await.expect("close");
        });

        let files: Vec<_> = fs::read_dir(temp_dir.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .collect();
        assert_eq!(files.len(), 2, "Expected two log files after date rotation");
    }

    #[test]
    fn test_size_based_rotation() {
        let temp_dir = setup_temp_dir();
        let transport = DailyRotateFile::builder()
            .filename(temp_dir.path().join("test.log"))
            .max_size(100)
            .build()
            .expect("Failed to create transport");

        drive(transport, |writer| async move {
            let log_message = "This is a test log message that should exceed the max file size.";
            for _ in 0..10 {
                writer
                    .write(fe("info", log_message))
                    .await
                    .expect("write");
            }
            writer.close().await.expect("close");
        });

        let files: Vec<_> = fs::read_dir(temp_dir.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .collect();

        assert_eq!(
            files.len(),
            10,
            "Expected 10 log files due to size rotation"
        );
    }

    #[test]
    fn test_compressed_archive() {
        let temp_dir = setup_temp_dir();
        let transport = DailyRotateFile::builder()
            .filename(temp_dir.path().join("test.log"))
            .max_size(80)
            .zipped_archive(true)
            .build()
            .expect("Failed to create transport");

        drive(transport, |writer| async move {
            for i in 0..5 {
                writer
                    .write(fe("info", format!("Test message {}", i)))
                    .await
                    .expect("write");
            }
            for i in 0..5 {
                writer
                    .write(fe("info", format!("Test message final {}", i)))
                    .await
                    .expect("write");
            }
            writer.close().await.expect("close");
        });

        let gz_files: Vec<_> = fs::read_dir(temp_dir.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .path()
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .map(|ext| ext == "gz")
                    .unwrap_or(false)
            })
            .collect();

        // Exact rotation count depends on per-line byte size (`info <msg>`
        // through Display); just verify zipping happened at all.
        assert!(
            !gz_files.is_empty(),
            "Expected at least one .gz file after size-driven rotations"
        );
    }

    #[test]
    fn test_max_files_cleanup() {
        let temp_dir = setup_temp_dir();
        let transport = DailyRotateFile::builder()
            .filename(temp_dir.path().join("test.log"))
            .date_pattern("%Y-%m-%d_%H-%M-%S")
            .max_files(2)
            .build()
            .expect("Failed to create transport");

        drive(transport, |writer| async move {
            for i in 0..5 {
                writer
                    .write(fe("info", format!("Message {}", i)))
                    .await
                    .expect("write");
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
            writer.close().await.expect("close");
        });

        let files: Vec<_> = fs::read_dir(temp_dir.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.path().is_file())
            .collect();

        assert_eq!(files.len(), 2, "Expected exactly 2 log files after cleanup");
    }

    /// Verifies `ingest_handle`: extract a handle, ingest a batch, confirm
    /// the entries land in the active file. (`X → DailyRotate` proxy target.)
    #[test]
    fn ingest_handle_appends_to_active_file() {
        use winston_transport::Transport as _;

        let temp_dir = setup_temp_dir();
        let log_path = temp_dir.path().join("ingest.log");
        let transport = DailyRotateFile::builder()
            .filename(&log_path)
            .date_pattern("%Y-%m-%d")
            .build()
            .expect("build transport");

        let active = transport
            .rotation_handle()
            .active_path
            .read()
            .unwrap()
            .clone();
        let handle = transport.ingest_handle().expect("ingest_handle is Some");

        futures::executor::block_on(async {
            handle
                .ingest(vec![
                    LogInfo::new("info", "ingested-1"),
                    LogInfo::new("warn", "ingested-2"),
                ])
                .await
                .expect("ingest");
        });
        drop(transport);

        let contents = std::fs::read_to_string(&active).unwrap();
        assert!(contents.contains("ingested-1"), "missing first: {contents:?}");
        assert!(contents.contains("ingested-2"), "missing second: {contents:?}");
    }

    /// Verifies that `rotation_handle().list_rotated_files()` returns every
    /// non-active log file produced by rotation, and that the active file
    /// (which is being written to) is excluded. The end-to-end shape:
    /// extract the handle, drive several rotations, list — confirm the
    /// listing matches the on-disk rotated files minus the active one.
    #[test]
    fn rotation_handle_lists_rotated_files() {
        let temp_dir = setup_temp_dir();
        let log_path = temp_dir.path().join("test.log");

        let transport = DailyRotateFile::builder()
            .filename(&log_path)
            .date_pattern("%Y-%m-%d_%H-%M-%S")
            .build()
            .expect("Failed to create transport");

        // Pull the handle off before the WritableStream consumes the transport.
        let rotation = transport.rotation_handle();

        // Force three rotations by writing across one-second boundaries.
        drive(transport, |writer| async move {
            writer
                .write(fe("info", "first"))
                .await
                .expect("write");
            std::thread::sleep(std::time::Duration::from_secs(1));
            writer
                .write(fe("info", "second"))
                .await
                .expect("write");
            std::thread::sleep(std::time::Duration::from_secs(1));
            writer
                .write(fe("info", "third"))
                .await
                .expect("write");
            writer.close().await.expect("close");
        });

        // Three writes across two rotation boundaries → three log files on
        // disk (one per second). After `close()`, the most recently active
        // file is the third one.
        let on_disk: Vec<_> = fs::read_dir(temp_dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_file())
            .map(|e| e.path())
            .collect();
        assert_eq!(on_disk.len(), 3, "expected three rotated files on disk");

        // The handle should list two of those three — everything except the
        // last-active.
        let rotated = rotation
            .list_rotated_files()
            .expect("list_rotated_files");
        assert_eq!(
            rotated.len(),
            2,
            "expected 2 rotated (non-active) files, got: {rotated:?}"
        );

        // None of the listed files should be the active path.
        let active = rotation.active_path.read().unwrap().clone();
        for path in &rotated {
            assert_ne!(path, &active, "list must exclude the active file");
        }
    }
}
