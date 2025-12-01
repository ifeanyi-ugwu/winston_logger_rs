#![cfg(feature = "log-backend")]

mod common;

use common::MockTransport;
use winston::Logger;

#[test]
fn test_log_backend_basic_integration() {
    let transport = MockTransport::new();

    let logger = Logger::builder().transport(transport.clone()).build();

    winston::init(logger);
    winston::register_with_log().expect("Failed to register with log crate");

    log::info!("Info from log crate");
    log::warn!("Warning from log crate");
    log::error!("Error from log crate");

    winston::flush().unwrap();

    assert_eq!(transport.log_count(), 3);
    assert!(transport.has_level("info"));
    assert!(transport.has_level("warn"));
    assert!(transport.has_level("error"));

    winston::close();
}

#[test]
fn test_log_backend_level_filtering() {
    let transport = MockTransport::new();

    let logger = Logger::builder()
        .level("warn")
        .transport(transport.clone())
        .build();

    winston::init(logger);
    winston::register_with_log().expect("Failed to register");

    log::trace!("Should be filtered");
    log::debug!("Should be filtered");
    log::info!("Should be filtered");
    log::warn!("Should pass");
    log::error!("Should pass");

    winston::flush().unwrap();

    assert_eq!(transport.log_count(), 2);
    let logs = transport.get_logs();
    assert_eq!(logs[0].level, "warn");
    assert_eq!(logs[1].level, "error");

    winston::close();
}

#[test]
fn test_log_backend_metadata_capture() {
    let transport = MockTransport::new();

    let logger = Logger::builder().transport(transport.clone()).build();

    winston::init(logger);
    winston::register_with_log().expect("Failed to register");

    log::info!("Test message");

    winston::flush().unwrap();

    let logs = transport.get_logs();
    assert_eq!(logs.len(), 1);

    // Should capture timestamp and target metadata
    assert!(logs[0].meta.contains_key("timestamp"));
    assert!(logs[0].meta.contains_key("target"));

    winston::close();
}

#[test]
fn test_log_backend_with_format() {
    let transport = MockTransport::new();

    let logger = Logger::builder()
        .transport(transport.clone())
        .format(winston::format::json())
        .build();

    winston::init(logger);
    winston::register_with_log().expect("Failed to register");

    log::info!("Formatted message");

    winston::flush().unwrap();

    assert_eq!(transport.log_count(), 1);

    winston::close();
}

#[test]
fn test_log_backend_enabled_check() {
    let transport = MockTransport::new();

    let logger = Logger::builder()
        .level("error")
        .transport(transport.clone())
        .build();

    winston::init(logger);
    winston::register_with_log().expect("Failed to register");

    // These should be filtered before reaching winston
    if log::log_enabled!(log::Level::Info) {
        log::info!("Should be filtered");
    }

    if log::log_enabled!(log::Level::Error) {
        log::error!("Should pass");
    }

    winston::flush().unwrap();

    // Only error should pass
    assert_eq!(transport.log_count(), 1);

    winston::close();
}

#[test]
fn test_log_backend_concurrent_logging() {
    let transport = MockTransport::new();

    let logger = Logger::builder().transport(transport.clone()).build();

    winston::init(logger);
    winston::register_with_log().expect("Failed to register");

    let handles: Vec<_> = (0..5)
        .map(|thread_id| {
            std::thread::spawn(move || {
                for i in 0..10 {
                    log::info!("Thread {} - Message {}", thread_id, i);
                }
            })
        })
        .collect();

    for handle in handles {
        handle.join().unwrap();
    }

    winston::flush().unwrap();

    assert_eq!(transport.log_count(), 50);

    winston::close();
}

#[test]
fn test_log_backend_mixed_with_winston() {
    let transport = MockTransport::new();

    let logger = Logger::builder().transport(transport.clone()).build();

    winston::init(logger);
    winston::register_with_log().expect("Failed to register");

    // Use both log crate and winston
    log::info!("From log crate");
    winston::log!(info, "From winston macro");

    winston::flush().unwrap();

    assert_eq!(transport.log_count(), 2);

    winston::close();
}

#[test]
#[cfg(feature = "log-backend-kv")]
fn test_log_backend_with_key_values() {
    use log::kv::Source;

    let transport = MockTransport::new();

    let logger = Logger::builder().transport(transport.clone()).build();

    winston::init(logger);
    winston::register_with_log().expect("Failed to register");

    log::info!(target: "test", user_id = 123; "User logged in");

    winston::flush().unwrap();

    let logs = transport.get_logs();
    assert_eq!(logs.len(), 1);

    // Should capture key-value pairs
    assert!(logs[0].meta.contains_key("user_id"));

    winston::close();
}

#[test]
fn test_log_backend_flush() {
    let transport = MockTransport::new();

    let logger = Logger::builder().transport(transport.clone()).build();

    winston::init(logger);
    winston::register_with_log().expect("Failed to register");

    log::info!("Test flush");

    // Call log's flush
    log::logger().flush();

    assert_eq!(transport.log_count(), 1);

    winston::close();
}
