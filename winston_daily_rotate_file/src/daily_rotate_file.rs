use chrono::{DateTime, Local, Utc};
use flate2::{write::GzEncoder, Compression};
use logform::{Format, LogInfo};
use std::fs::{create_dir_all, read_dir, File, OpenOptions};
use std::io::{BufWriter, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use winston_transport::Transport;

pub struct DailyRotateFileOptions {
    pub level: Option<String>,
    pub format: Option<Arc<dyn Format<Input = LogInfo> + Send + Sync>>,
    pub filename: PathBuf,
    pub date_pattern: String,
    pub max_files: Option<u32>,
    pub max_size: Option<u64>, // in bytes
    pub dirname: Option<PathBuf>,
    pub zipped_archive: bool,
    pub utc: bool,
}

pub struct DailyRotateFile {
    file: Mutex<BufWriter<File>>,
    options: DailyRotateFileOptions,
    last_rotation: Mutex<DateTime<Utc>>,
    file_path: Mutex<PathBuf>,
}

impl DailyRotateFile {
    pub fn new(options: DailyRotateFileOptions) -> Self {
        let current_date = if options.utc {
            Utc::now()
        } else {
            Local::now().with_timezone(&Utc)
        };

        let (file, path) =
            Self::create_file(&options, &current_date).expect("Failed to create initial log file");

        DailyRotateFile {
            file: Mutex::new(BufWriter::new(file)),
            options,
            last_rotation: Mutex::new(current_date),
            file_path: Mutex::new(path),
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

    fn get_file_size(&self) -> u64 {
        self.file
            .lock()
            .ok()
            .and_then(|mut file_guard| {
                file_guard.flush().ok()?;
                file_guard.get_ref().metadata().ok().map(|m| m.len())
            })
            .unwrap_or(0)
    }

    fn should_rotate(&self, new_entry_size: usize) -> bool {
        let now = Utc::now();

        let now_str = if self.options.utc {
            now.format(&self.options.date_pattern).to_string()
        } else {
            now.with_timezone(&Local)
                .format(&self.options.date_pattern)
                .to_string()
        };

        let last_rotation = self.last_rotation.lock().unwrap();
        let last_rotation_str = if self.options.utc {
            last_rotation.format(&self.options.date_pattern).to_string()
        } else {
            last_rotation
                .with_timezone(&Local)
                .format(&self.options.date_pattern)
                .to_string()
        };

        if last_rotation_str != now_str {
            return true;
        }

        self.options
            .max_size
            .map(|max_size| self.get_file_size() + new_entry_size as u64 >= max_size)
            .unwrap_or(false)
    }

    fn rotate(&self) {
        let now = Utc::now();

        if let Ok(mut file_guard) = self.file.lock() {
            let _ = file_guard.flush();
        }

        let previous_file_path = self.file_path.lock().unwrap().clone();

        let (new_file, new_path) =
            Self::create_file(&self.options, &now).expect("Failed to rotate log file");

        // Replace the existing file with the new one
        if let Ok(mut file_lock) = self.file.lock() {
            *file_lock = BufWriter::new(new_file);
        }

        if let Ok(mut path_lock) = self.file_path.lock() {
            *path_lock = new_path;
        }

        if let Ok(mut last_rotation) = self.last_rotation.lock() {
            *last_rotation = now;
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
        //println!("cleaning up");

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

        // add all log files, zipped ones inclusive
        for entry in read_dir(log_dir)? {
            let entry = entry?;
            let path = entry.path();

            if path.is_file() {
                let filename = path.file_name().and_then(|s| s.to_str()).unwrap_or("");

                //println!("Checking file: {}", filename);
                //println!("Base name: {}", base_name);

                // Check if it's one of our log files ("basename.date" and "basename_N.date" formats)
                if filename.starts_with(&format!("{}.", base_name))
                    || filename.starts_with(&format!("{}_", base_name))
                {
                    log_files.push(path);
                }
            }
        }

        //println!("log files found: {:?}", log_files);

        if log_files.len() <= max_files as usize {
            return Ok(());
        }

        // Sort by modification time (newest first)
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

        for file in &log_files {
            println!("Detected log file: {}", file.display());
        }

        // Keep only max_files
        for old_file in log_files.iter().skip(max_files as usize) {
            //println!("Deleting file: {}", old_file.display());

            // don't delete active log file
            let current_path = self.file_path.lock().map(|p| p.clone()).unwrap_or_default();
            if old_file == &current_path {
                continue;
            }

            if self.options.zipped_archive
                && old_file.extension().and_then(|e| e.to_str()) != Some("gz")
            {
                // compress_file also deletes the original file
                //let _ = Self::compress_file(old_file);
                if let Err(e) = Self::compress_file(old_file) {
                    eprintln!("Failed to compress old file {}: {}", old_file.display(), e);
                }
            } else {
                //let _ = std::fs::remove_file(old_file);
                if let Err(e) = std::fs::remove_file(old_file) {
                    eprintln!("Failed to remove old file {}: {}", old_file.display(), e);
                }
            }
        }

        Ok(())
    }

    pub fn builder() -> DailyRotateFileBuilder {
        DailyRotateFileBuilder::new()
    }
}

impl Transport<LogInfo> for DailyRotateFile {
    fn log(&self, info: LogInfo) {
        let entry_size = format!("{}\n", info.message).len();

        if self.should_rotate(entry_size) {
            self.rotate();
        }
        //println!("File size before: {}", self.get_file_size());

        let mut file = match self.file.lock() {
            Ok(f) => f,
            Err(_) => {
                eprintln!("Failed to acquire file lock");
                return;
            }
        };

        if let Err(e) = writeln!(file, "{}", info.message) {
            eprintln!("Failed to write log: {}", e);
        }

        //drop(file);

        //println!("File size after: {}", self.get_file_size()); //deadlocks
    }

    fn log_batch(&self, infos: Vec<LogInfo>) {
        if infos.is_empty() {
            return;
        }

        // Calculate the total size of the batch to determine if rotation is needed before writing
        let total_batch_size: usize = infos
            .iter()
            .map(|info| format!("{}\n", info.message).len())
            .sum();

        if self.should_rotate(total_batch_size) {
            self.rotate();
        }

        let mut file = match self.file.lock() {
            Ok(f) => f,
            Err(_) => {
                eprintln!("Failed to acquire file lock for batch logging");
                return;
            }
        };

        for info in infos {
            if let Err(e) = writeln!(file, "{}", info.message) {
                eprintln!("Failed to write log entry in batch: {}", e);
            }
        }
    }

    fn flush(&self) -> Result<(), String> {
        let mut file = self.file.lock().unwrap();
        file.flush().map_err(|e| format!("Failed to flush: {}", e))
    }
}

pub struct DailyRotateFileBuilder {
    level: Option<String>,
    format: Option<Arc<dyn Format<Input = LogInfo> + Send + Sync>>,
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
            level: None,
            format: None,
            filename: None,
            date_pattern: String::from("%Y-%m-%d"),
            max_files: None,
            max_size: None,
            dirname: None,
            zipped_archive: false,
            utc: false,
        }
    }

    pub fn level<T: Into<String>>(mut self, level: T) -> Self {
        self.level = Some(level.into());
        self
    }

    pub fn format(mut self, format: Arc<dyn Format<Input = LogInfo> + Send + Sync>) -> Self {
        self.format = Some(format);
        self
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
            level: self.level,
            format: self.format,
            filename,
            date_pattern: self.date_pattern,
            max_files: self.max_files,
            max_size: self.max_size,
            dirname: self.dirname,
            zipped_archive: self.zipped_archive,
            utc: self.utc,
        };

        Ok(DailyRotateFile::new(options))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Local;
    use std::fs;
    use tempfile::TempDir;

    fn setup_temp_dir() -> TempDir {
        let project_root = std::env::current_dir().expect("Failed to get current directory");
        TempDir::new_in(&project_root).expect("Failed to create temp directory in project folder")
    }

    fn create_test_transport(temp_dir: &TempDir) -> DailyRotateFile {
        let log_path = temp_dir.path().join("test.log");
        DailyRotateFile::builder()
            .filename(&log_path)
            .date_pattern("%Y-%m-%d")
            .max_files(3)
            .max_size(1024) // 1KB
            .build()
            .expect("Failed to create transport")
    }

    #[test]
    fn test_basic_logging() {
        let temp_dir = setup_temp_dir();
        let transport = create_test_transport(&temp_dir);

        let log_info = LogInfo {
            level: "info".to_string(),
            message: "Test message".to_string(),
            meta: Default::default(),
        };

        transport.log(log_info);
        transport.flush().expect("Failed to flush");

        // Check if log file exists and contains the message
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

        transport.log(LogInfo {
            level: "info".to_string(),
            message: "log entry 1".to_string(),
            meta: Default::default(),
        });

        // Simulate date change
        std::thread::sleep(std::time::Duration::from_secs(1));

        transport.log(LogInfo {
            level: "info".to_string(),
            message: "log entry 2".to_string(),
            meta: Default::default(),
        });

        transport.flush().expect("Failed to flush");

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
            //.filename("logs/test.log")
            .max_size(100)
            .build()
            .expect("Failed to create transport");

        let log_message = "This is a test log message that should exceed the max file size.";
        let log_info = LogInfo {
            level: "info".to_string(),
            message: log_message.to_string(),
            meta: Default::default(),
        };

        // Write multiple logs until rotation occurs
        for _ in 0..10 {
            transport.log(log_info.clone());
        }

        transport.flush().expect("Failed to flush");

        // Check if multiple log files were created
        let files: Vec<_> = fs::read_dir(temp_dir.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .collect();

        //println!("{}", files.len());
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
            //.filename("logs/test.log")
            .max_size(80) // Small size to force rotation `Test message x` plus new line is 15 bytes each * 5 = 75 + 5 buffer
            .zipped_archive(true)
            .build()
            .expect("Failed to create transport");

        // Create log entries to force rotation
        for i in 0..5 {
            let log_info = LogInfo {
                level: "info".to_string(),
                message: format!("Test message {}", i),
                meta: Default::default(),
            };
            transport.log(log_info);
        }

        // Add more entries to trigger another rotation
        // this entry will hit max size at about the 3rd message
        // which means it will cause a rotation and still keep an open file containing the last 2 messages
        for i in 0..5 {
            let log_info = LogInfo {
                level: "info".to_string(),
                message: format!("Test message final {}", i),
                meta: Default::default(),
            };
            transport.log(log_info);
        }

        // Check if .gz files were created
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
            //.filename("logs/test.log")
            .date_pattern("%Y-%m-%d_%H-%M-%S")
            .max_files(2)
            .build()
            .expect("Failed to create transport");

        for i in 0..5 {
            transport.log(LogInfo {
                level: "info".to_string(),
                message: format!("Message {}", i),
                meta: Default::default(),
            });

            // simulate date change
            std::thread::sleep(std::time::Duration::from_secs(1));
        }

        transport.flush().expect("Failed to flush");

        let files: Vec<_> = fs::read_dir(temp_dir.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.path().is_file())
            .collect();

        assert_eq!(files.len(), 2, "Expected exactly 2 log files after cleanup");
    }
}
