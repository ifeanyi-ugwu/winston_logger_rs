mod common;

use common::{wait_for_logs, MockTransport};
use logform::LogInfo;
use std::sync::Arc;
use winston::{log, BackpressureStrategy, Logger, LoggerOptions};

#[test]
fn test_logger_builder_api() {
    let transport = MockTransport::new();

    let logger = Logger::builder()
        .level("info")
        .channel_capacity(512)
        .backpressure_strategy(BackpressureStrategy::Block)
        .transport(transport.clone())
        .build();

    logger.log(LogInfo::new("info", "Test message"));
    wait_for_logs(&logger);

    assert!(transport.has_message("Test message"));
}

#[test]
fn test_logger_options_api() {
    let transport = MockTransport::new();

    let options = LoggerOptions::new()
        .level("debug")
        .transport(transport.clone());

    let logger = Logger::new(Some(options));

    logger.log(LogInfo::new("debug", "Debug message"));
    wait_for_logs(&logger);

    assert_eq!(transport.log_count(), 1);
}

#[test]
fn test_log_macro_with_logger_instance() {
    let transport = MockTransport::new();
    let logger = Logger::builder().transport(transport.clone()).build();

    log!(logger, info, "Simple message");
    log!(
        logger,
        warn,
        "Message with metadata",
        key = "value",
        count = 42
    );

    wait_for_logs(&logger);

    assert_eq!(transport.log_count(), 2);
    assert!(transport.has_level("info"));
    assert!(transport.has_level("warn"));
}

#[test]
fn test_log_macro_formats_message() {
    let transport = MockTransport::new();
    let logger = Logger::builder().transport(transport.clone()).build();

    let user_id = 123;
    let message = format!("User {} logged in", user_id);
    log!(logger, info, message);

    wait_for_logs(&logger);

    assert!(transport.has_message("User 123 logged in"));
}

#[test]
fn test_logger_with_multiple_transports() {
    let transport1 = MockTransport::new();
    let transport2 = MockTransport::new();

    let logger = Logger::builder()
        .transport(transport1.clone())
        .transport(transport2.clone())
        .build();

    log!(logger, info, "Broadcast message");
    wait_for_logs(&logger);

    assert!(transport1.has_message("Broadcast message"));
    assert!(transport2.has_message("Broadcast message"));
}

#[test]
fn test_add_transport_at_runtime() {
    let logger = Logger::builder().build();
    let transport = MockTransport::new();

    // Add transport after creation
    logger.add_transport(transport.clone());

    log!(logger, info, "Runtime transport");
    wait_for_logs(&logger);

    assert_eq!(transport.log_count(), 1);
}

#[test]
fn test_remove_transport_at_runtime() {
    let transport = MockTransport::new();

    let logger = Logger::builder() /*.add_transport(transport.clone())*/
        .build();
    let transport_handle = logger.add_transport(transport.clone());

    log!(logger, info, "Before removal");
    wait_for_logs(&logger);
    assert_eq!(transport.log_count(), 1);

    // Remove transport
    logger.remove_transport(transport_handle);
    transport.clear_logs();

    log!(logger, info, "After removal");
    wait_for_logs(&logger);
    assert_eq!(transport.log_count(), 0);
}

#[test]
fn test_configure_updates_logger() {
    let transport = MockTransport::new();
    let logger = Logger::builder()
        .level("error")
        .transport(transport.clone())
        .build();

    log!(logger, warn, "Should be filtered");
    wait_for_logs(&logger);

    // Reconfigure
    logger.configure(Some(
        LoggerOptions::new()
            .level("debug")
            .transport(transport.clone()),
    ));

    log!(logger, warn, "Should pass now");
    wait_for_logs(&logger);

    assert_eq!(transport.log_count(), 1);
}

#[test]
fn test_query_with_level_filter() {
    let transport = MockTransport::new();
    let logger = Logger::builder()
        .format(logform::timestamp())
        .transport(transport.clone())
        .build();

    log!(logger, info, "Info message");
    log!(logger, error, "Error message");
    log!(logger, warn, "Warning message");
    wait_for_logs(&logger);

    let query = winston::LogQuery::new().levels(vec!["error"]);
    let results = logger.query(&query).unwrap();

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].level, "error");
}

#[test]
fn test_flush_ensures_delivery() {
    let transport = MockTransport::new();
    let logger = Logger::builder().transport(transport.clone()).build();

    for i in 0..10 {
        log!(logger, info, format!("Message {}", i));
    }

    logger.flush().expect("Flush should succeed");

    assert_eq!(transport.log_count(), 10);
}

#[test]
fn test_close_flushes_pending_logs() {
    let transport = MockTransport::new();
    let logger = Logger::builder().transport(transport.clone()).build();

    log!(logger, info, "Final message");
    logger.close();

    assert_eq!(transport.log_count(), 1);
}

#[test]
fn test_default_logger_creation() {
    let logger = Logger::default();
    let transport = MockTransport::new();
    logger.add_transport(transport.clone());

    log!(logger, info, "Using default logger");
    wait_for_logs(&logger);

    assert!(transport.has_message("Using default logger"));
}

#[test]
fn test_level_hierarchy() {
    let transport = MockTransport::new();
    let logger = Logger::builder()
        .level("warn")
        .transport(transport.clone())
        .build();

    log!(logger, trace, "Trace - filtered");
    log!(logger, debug, "Debug - filtered");
    log!(logger, info, "Info - filtered");
    log!(logger, warn, "Warn - passes");
    log!(logger, error, "Error - passes");
    wait_for_logs(&logger);

    assert_eq!(transport.log_count(), 2);
    let logs = transport.get_logs();
    assert_eq!(logs[0].level, "warn");
    assert_eq!(logs[1].level, "error");
}

#[test]
fn test_metadata_preservation() {
    let transport = MockTransport::new();
    let logger = Logger::builder()
        .format(logform::passthrough())
        .transport(transport.clone())
        .build();

    log!(
        logger,
        info,
        "With metadata",
        user_id = 123,
        action = "login"
    );
    wait_for_logs(&logger);

    let logs = transport.get_logs();
    assert_eq!(logs.len(), 1);
    assert!(logs[0].meta.contains_key("user_id"));
    assert!(logs[0].meta.contains_key("action"));
}

#[test]
fn test_empty_message_filtered() {
    let transport = MockTransport::new();
    let logger = Logger::builder().transport(transport.clone()).build();

    log!(logger, info, "");
    wait_for_logs(&logger);

    assert_eq!(transport.log_count(), 0);
}
