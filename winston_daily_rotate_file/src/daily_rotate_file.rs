use chrono::{DateTime, Local, Utc};
use flate2::{write::GzEncoder, Compression};
use logform::LogInfo;
use std::fs::{create_dir_all, read_dir, File, OpenOptions};
use std::io::{BufWriter, ErrorKind, Write};
use std::path::{Path, PathBuf};
use whatwg_streams::{StreamResult, WritableSink, WritableStreamDefaultController};
use winston_transport::Transport;

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
            file_path: path,
        })
    }

    pub fn builder() -> DailyRotateFileBuilder {
        DailyRotateFileBuilder::new()
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
        self.file_path = new_path;
        self.last_rotation = now;

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

impl WritableSink<LogInfo> for DailyRotateFile {
    async fn write(
        &mut self,
        info: LogInfo,
        _controller: &mut WritableStreamDefaultController,
    ) -> StreamResult<()> {
        let entry_size = format!("{}\n", info.message).len();
        if self.should_rotate(entry_size) {
            self.rotate()?;
        }
        writeln!(&mut self.writer, "{}", info.message)?;
        Ok(())
    }

    async fn close(mut self) -> StreamResult<()> {
        self.writer.flush()?;
        Ok(())
    }
}

impl Transport for DailyRotateFile {
    // Query is intentionally unsupported — the rotation/archive lifecycle
    // means past entries live in N files (and possibly .gz archives) that the
    // transport doesn't track centrally. If you need query, log to a regular
    // `winston_file::FileTransport` (which keeps a single file) alongside.
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
            whatwg_streams::WritableStreamDefaultWriter<LogInfo, DailyRotateFile>,
        ) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let stream = WritableStream::builder(transport)
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
                .write(LogInfo::new("info", "Test message"))
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
                .write(LogInfo::new("info", "log entry 1"))
                .await
                .expect("write");
            // Simulate date change — `should_rotate` looks at the formatted
            // current time vs. last rotation, so a 1s sleep with a per-second
            // pattern is enough to trigger.
            std::thread::sleep(std::time::Duration::from_secs(1));
            writer
                .write(LogInfo::new("info", "log entry 2"))
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
                    .write(LogInfo::new("info", log_message))
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
                    .write(LogInfo::new("info", format!("Test message {}", i)))
                    .await
                    .expect("write");
            }
            for i in 0..5 {
                writer
                    .write(LogInfo::new("info", format!("Test message final {}", i)))
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

        assert!(gz_files.len() == 2, "Expected 2 .gz files");
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
                    .write(LogInfo::new("info", format!("Message {}", i)))
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
}
