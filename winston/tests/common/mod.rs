use logform::LogInfo;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use winston_transport::{LogQuery, Transport};

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

/// A comprehensive mock transport for testing
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

impl Transport<LogInfo> for MockTransport {
    fn log(&self, info: LogInfo) {
        if self.config.should_fail_log {
            return;
        }

        if self.config.delay > Duration::from_millis(0) {
            thread::sleep(self.config.delay);
        }

        self.logs.lock().unwrap().push(info);
    }

    fn flush(&self) -> Result<(), String> {
        if self.config.should_fail_flush {
            Err("Mock flush failure".to_string())
        } else {
            Ok(())
        }
    }

    fn query(&self, options: &LogQuery) -> Result<Vec<LogInfo>, String> {
        let logs = self.logs.lock().unwrap();
        Ok(logs
            .iter()
            .filter(|log| options.matches(log))
            .cloned()
            .collect())
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
