//use std::collections::HashMap;
use logform::{Format, LogInfo};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use winston_proxy_transport::Proxy;
use winston_transport::{LogQuery, Transport};

pub struct FileTransportOptions {
    pub level: Option<String>,
    pub format: Option<Arc<dyn Format<Input = LogInfo> + Send + Sync>>,
    pub filename: Option<PathBuf>,
    /*
    unused yet
    pub dirname: Option<String>,
    pub options: Option<HashMap<String, String>>,
    pub maxsize: Option<u64>,
    pub stream: Option<Box<dyn Write + Send + Sync>>,
    pub rotation_format: Option<Box<dyn Fn() -> String + Send + Sync>>,
    pub zipped_archive: Option<bool>,
    pub max_files: Option<u64>,
    pub eol: Option<String>,
    pub tailable: Option<bool>,
    pub lazy: Option<bool>,
     */
}

pub struct FileTransport {
    file: Mutex<BufWriter<File>>,
    options: FileTransportOptions,
    proxy_lock: Mutex<()>,
}

impl FileTransport {
    pub fn new(options: FileTransportOptions) -> Self {
        let file_path = options
            .filename
            .clone()
            .expect("File path is required for FileTransport");
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(file_path)
            .expect("Failed to open log file");
        let writer = BufWriter::new(file);

        FileTransport {
            file: Mutex::new(writer),
            options,
            proxy_lock: Mutex::new(()),
        }
    }

    pub fn builder() -> FileTransportBuilder {
        FileTransportBuilder::new()
    }

    /*unused
    pub fn flush(&self) -> std::io::Result<()> {
           let mut file = self.file.lock().unwrap();
           file.flush()
       }
    */
}

impl FileTransport {
    fn parse_log_entry(&self, line: &str) -> Option<LogInfo> {
        let parsed: serde_json::Value = serde_json::from_str(line).ok()?;
        //println!("Parsed log entry: {:?}", parsed); // Debug print

        let level = parsed["level"].as_str()?;
        let message = parsed["message"].as_str()?;
        let meta = parsed
            .as_object()?
            .iter()
            //.map(|(k, v)| (k.clone(), v.clone()))
            .filter_map(|(k, v)| {
                if k != "level" && k != "message" {
                    Some((k.clone(), v.clone()))
                } else {
                    None
                }
            })
            .collect::<HashMap<_, _>>(); // Collect all metadata

        Some(LogInfo {
            level: level.to_string(),
            message: message.to_string(),
            meta,
        })
    }
}

impl Transport<LogInfo> for FileTransport {
    /*fn log(&self, message: &str, _level: &str) {
        let mut file = self.file.lock().unwrap();

        writeln!(file, "{}", message).unwrap();
    } */

    fn log(&self, info: LogInfo) {
        let mut file = self.file.lock().unwrap();
        if let Err(e) = writeln!(file, "{}", info.message) {
            eprintln!("Failed to write to log file: {}", e);
        }
    }

    fn log_batch(&self, logs: Vec<LogInfo>) {
        let mut file = self.file.lock().unwrap();
        for info in logs {
            if let Err(e) = writeln!(file, "{}", info.message) {
                eprintln!("Failed to write to log file in batch: {}", e);
            }
        }
    }

    fn flush(&self) -> Result<(), String> {
        let mut file = self.file.lock().unwrap();
        //println!("Flushing file transport");

        file.flush()
            .map_err(|e| format!("Failed to flush file: {}", e))
    }

    fn query(&self, query: &LogQuery) -> Result<Vec<LogInfo>, String> {
        let file = File::open(self.options.filename.as_ref().unwrap())
            .map_err(|e| format!("Failed to open log file: {}", e))?;
        let reader = BufReader::new(file);

        let mut results = Vec::new();

        // Determine the start and limit values
        let start = query.start.unwrap_or(0);
        let limit = query.limit.unwrap_or(usize::MAX);

        for (index, line) in reader.lines().enumerate() {
            let line = line.map_err(|e| format!("Failed to read line {}: {}", index, e))?;
            if let Some(entry) = self.parse_log_entry(&line) {
                if query.matches(&entry) {
                    // Skip lines until the start position
                    if index >= start {
                        results.push(entry);
                    }

                    // Stop reading if the limit is reached
                    if results.len() >= limit && limit != 0 {
                        break;
                    }
                }
            }
        }

        // Apply sorting to the results
        query.sort(&mut results);

        // Project fields if specified
        let results = if !query.fields.is_empty() {
            results
                .into_iter()
                .map(|entry| {
                    // Normalize fields to lowercase for case-insensitive matching
                    let normalized_fields: Vec<String> =
                        query.fields.iter().map(|f| f.to_lowercase()).collect();

                    LogInfo {
                        // Only include level if 'level' is in fields
                        level: if normalized_fields.contains(&"level".to_string()) {
                            entry.level
                        } else {
                            String::new()
                        },
                        // Only include message if 'message' is in fields
                        message: if normalized_fields.contains(&"message".to_string()) {
                            entry.message
                        } else {
                            String::new()
                        },
                        // Filter meta fields based on specified fields
                        meta: entry
                            .meta
                            .into_iter()
                            .filter(|(k, _)| normalized_fields.contains(&k.to_lowercase()))
                            .collect(),
                    }
                })
                .collect()
        } else {
            results
        };

        //println!("results: {:?}", results);
        Ok(results)
    }
}

impl Drop for FileTransport {
    fn drop(&mut self) {
        // Attempt to flush any remaining logs before dropping
        if let Ok(mut file) = self.file.lock() {
            if let Err(e) = file.flush() {
                eprintln!("Error flushing log file during drop: {}", e);
            }
        }
    }
}

impl Proxy<LogInfo> for FileTransport {
    fn proxy(&self, target: &dyn Proxy<LogInfo>) -> Result<usize, String> {
        let _lock = self
            .proxy_lock
            .lock()
            .map_err(|_| "Failed to acquire proxy lock")?;

        let log_file_path = self
            .options
            .filename
            .as_ref()
            .ok_or("No file path provided")?;

        // Generate backup path
        let mut counter = 0;
        let backup_path = loop {
            let candidate = log_file_path.with_extension(format!("bak{}", counter));
            if !candidate.exists() {
                break candidate;
            }
            counter += 1;
        };

        // Lock file and flush pending writes
        {
            let mut file_guard = self
                .file
                .lock()
                .map_err(|_| "Failed to acquire file lock")?;
            file_guard
                .flush()
                .map_err(|e| format!("Failed to flush pending writes: {}", e))?;
        } // Drop the lock to release the file handle

        // Rename file
        std::fs::rename(log_file_path, &backup_path)
            .map_err(|e| format!("Failed to rename file: {}", e))?;

        // Create new log file and update the BufWriter
        let new_log_file = File::create(log_file_path)
            .map_err(|e| format!("Failed to create new log file: {}", e))?;

        // Replace the old BufWriter with a new one pointing to the new file
        {
            let mut file_guard = self
                .file
                .lock()
                .map_err(|_| "Failed to acquire file lock")?;
            *file_guard = BufWriter::new(new_log_file);
        }

        // Open the backup log file for streaming
        let file =
            File::open(&backup_path).map_err(|e| format!("Failed to open backup log: {}", e))?;
        let mut reader = BufReader::new(file);
        let mut line = String::new();
        let mut log_count = 0;

        // Read line by line and send immediately
        while reader
            .read_line(&mut line)
            .map_err(|e| format!("Failed to read log line: {}", e))?
            > 0
        {
            if let Some(log) = self.parse_log_entry(&line) {
                target.ingest(vec![log])?; // Directly send each log
                log_count += 1;
            }
            line.clear(); // Clear buffer for next line
        }

        // Delete backup file after processing
        std::fs::remove_file(&backup_path)
            .map_err(|e| format!("Failed to delete backup file: {}", e))?;

        Ok(log_count)
    }

    fn ingest(&self, logs: Vec<LogInfo>) -> Result<(), String> {
        let mut file = self
            .file
            .lock()
            .map_err(|e| format!("Failed to acquire file lock for ingest: {}", e))?;

        for log in logs {
            let formatted_log = self
                .options
                .format
                .as_ref()
                .map(|format| format.transform(log.clone()))
                .unwrap_or(Some(log))
                .ok_or_else(|| "Transform failed".to_string())?;

            writeln!(file, "{}", formatted_log.message)
                .map_err(|e| format!("Failed to write log: {}", e))?;
        }

        // Flush after writing batch
        file.flush()
            .map_err(|e| format!("Failed to flush after ingest: {}", e))?;
        Ok(())
    }
}

pub struct FileTransportBuilder {
    level: Option<String>,
    format: Option<Arc<dyn Format<Input = LogInfo> + Send + Sync>>,
    filename: Option<PathBuf>,
}

impl FileTransportBuilder {
    pub fn new() -> Self {
        Self {
            level: None,
            format: None,
            filename: None,
        }
    }

    pub fn level<T: Into<String>>(mut self, level: T) -> Self {
        self.level = Some(level.into());
        self
    }

    pub fn format<F>(mut self, format: F) -> Self
    where
        F: Format<Input = LogInfo> + Send + Sync + 'static,
    {
        self.format = Some(Arc::new(format));
        self
    }

    pub fn filename<T: Into<PathBuf>>(mut self, filename: T) -> Self {
        self.filename = Some(filename.into());
        self
    }

    pub fn build(self) -> FileTransport {
        let options = FileTransportOptions {
            level: self.level,
            format: self.format,
            filename: self.filename,
            // Set other fields as needed
        };
        FileTransport::new(options)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use logform::{json, timestamp};
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;
    use winston_proxy_transport::ProxyTransport;

    #[test]
    fn test_file_transport_proxy() -> Result<(), String> {
        // Clean up any existing test files
        let _ = std::fs::remove_file("test_source.log");
        let _ = std::fs::remove_file("test_target.log");

        let source_transport =
            Arc::new(FileTransport::builder().filename("test_source.log").build());
        let target_transport = Arc::new(
            FileTransport::builder()
                .filename("test_target.log")
                .format(json())
                .build(),
        );

        let proxy_interval = Duration::from_secs(1);
        let proxy_transport = ProxyTransport::new(
            source_transport.clone(),
            target_transport.clone(),
            proxy_interval,
        );

        let log = LogInfo::new("info", "Test message");
        let log = timestamp().transform(log.clone()).unwrap();
        let log = json().transform(log.clone()).unwrap();

        proxy_transport.log(log);

        // Wait for the proxying to complete
        thread::sleep(proxy_interval * 2);

        let source_logs_after = source_transport.query(&LogQuery::new())?;
        let target_logs_after = target_transport.query(&LogQuery::new())?;

        assert!(
            source_logs_after.is_empty(),
            "Source log file should be empty after proxying"
        );
        assert_eq!(
            target_logs_after.len(),
            1,
            "Target log file should contain the proxied log"
        );

        // Clean up after test
        let _ = std::fs::remove_file("test_source.log");
        let _ = std::fs::remove_file("test_target.log");
        Ok(())
    }
}
