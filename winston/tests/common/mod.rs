#![allow(dead_code)]

use logform::{FormattedEntry, LogInfo};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use whatwg_streams::{
    ReadableSource, ReadableStreamDefaultController, StreamResult, WritableSink,
    WritableStreamDefaultController,
};
use winston_transport::{DynQueryHandle, DynReadableSource, LogQuery, Transport};

/// Configuration for MockTransport behavior
#[derive(Clone, Debug)]
pub struct MockConfig {
    pub delay: Duration,
    pub should_fail_log: bool,
    pub should_fail_flush: bool,
    pub level: Option<String>,
}

impl Default for MockConfig {
    fn default() -> Self {
        Self {
            delay: Duration::from_millis(0),
            should_fail_log: false,
            should_fail_flush: false,
            level: None,
        }
    }
}

/// A comprehensive mock transport for testing.
///
/// Implements the new `Transport: WritableSink<LogInfo>` contract. Cloning
/// shares the underlying `Arc<Mutex<Vec<LogInfo>>>` so the test thread keeps
/// a handle to inspect what got written, even after the Logger consumes the
/// transport into its `WritableStream`.
#[derive(Clone, Debug)]
pub struct MockTransport {
    pub logs: Arc<Mutex<Vec<LogInfo>>>,
    config: MockConfig,
}

impl MockTransport {
    pub fn new() -> Self {
        Self {
            logs: Arc::new(Mutex::new(Vec::new())),
            config: MockConfig::default(),
        }
    }

    pub fn with_config(config: MockConfig) -> Self {
        Self {
            logs: Arc::new(Mutex::new(Vec::new())),
            config,
        }
    }

    pub fn with_delay(delay: Duration) -> Self {
        Self::with_config(MockConfig {
            delay,
            ..Default::default()
        })
    }

    pub fn get_logs(&self) -> Vec<LogInfo> {
        self.logs.lock().unwrap().clone()
    }

    pub fn clear_logs(&self) {
        self.logs.lock().unwrap().clear();
    }

    pub fn log_count(&self) -> usize {
        self.logs.lock().unwrap().len()
    }

    pub fn has_message(&self, message: &str) -> bool {
        self.logs
            .lock()
            .unwrap()
            .iter()
            .any(|log| log.message.contains(message))
    }

    pub fn has_level(&self, level: &str) -> bool {
        self.logs
            .lock()
            .unwrap()
            .iter()
            .any(|log| log.level == level)
    }
}

impl WritableSink<FormattedEntry> for MockTransport {
    async fn write(
        &mut self,
        entry: FormattedEntry,
        _controller: &mut WritableStreamDefaultController,
    ) -> StreamResult<()> {
        if self.config.should_fail_log {
            return Ok(());
        }
        if self.config.delay > Duration::from_millis(0) {
            thread::sleep(self.config.delay);
        }
        self.logs.lock().unwrap().push(entry.info);
        Ok(())
    }
}

impl Transport for MockTransport {
    fn query_handle(&self) -> Option<Box<dyn DynQueryHandle>> {
        Some(Box::new(MockQueryHandle {
            logs: Arc::clone(&self.logs),
        }))
    }
}

struct MockQueryHandle {
    logs: Arc<Mutex<Vec<LogInfo>>>,
}

impl DynQueryHandle for MockQueryHandle {
    fn query(&self, options: &LogQuery) -> Option<Box<dyn DynReadableSource>> {
        let snapshot: Vec<LogInfo> = {
            let logs = self.logs.lock().unwrap();
            logs.iter()
                .filter(|log| {
                    if !options.levels.is_empty() && !options.levels.contains(&log.level) {
                        return false;
                    }
                    if let Some(ref filter) = options.filter {
                        if !filter.evaluate(&log.to_flat_value()) {
                            return false;
                        }
                    }
                    true
                })
                .cloned()
                .collect()
        };
        Some(Box::new(VecSource {
            entries: snapshot.into_iter(),
        }))
    }
}

struct VecSource {
    entries: std::vec::IntoIter<LogInfo>,
}

impl ReadableSource<LogInfo> for VecSource {
    async fn pull(
        &mut self,
        controller: &mut ReadableStreamDefaultController<LogInfo>,
    ) -> StreamResult<()> {
        match self.entries.next() {
            Some(entry) => {
                let _ = controller.enqueue(entry);
            }
            None => {
                let _ = controller.close();
            }
        }
        Ok(())
    }
}

/// Helper to generate unique test file paths
pub fn temp_log_file() -> String {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let count = COUNTER.fetch_add(1, Ordering::SeqCst);
    format!("test_log_{}_{}.log", std::process::id(), count)
}

/// Helper to cleanup test files
pub fn cleanup_file(path: &str) {
    let _ = std::fs::remove_file(path);
}

/// Helper to wait for async log processing
pub fn wait_for_logs(logger: &winston::Logger) {
    logger.flush().expect("Failed to flush logger");
}

/// Block on a Logger::query call. Tests that don't have an async runtime call
/// query through this helper.
pub fn query_blocking(
    logger: &winston::Logger,
    options: &LogQuery,
) -> Result<Vec<LogInfo>, String> {
    futures::executor::block_on(logger.query(options))
}
