mod common;

use common::{MockConfig, MockTransport};
use logform::LogInfo;
use winston::Logger;

#[test]
fn test_transport_log_failure_does_not_crash() {
    let config = MockConfig {
        should_fail_log: true,
        ..Default::default()
    };
    let failing_transport = MockTransport::with_config(config);
    let working_transport = MockTransport::new();

    let logger = Logger::builder()
        .transport(failing_transport.clone())
        .transport(working_transport.clone())
        .build();

    logger.log(LogInfo::new("info", "Test message"));
    logger.flush().unwrap();

    // Failing transport should have 0 logs
    assert_eq!(failing_transport.log_count(), 0);

    // Working transport should still receive the log
    assert_eq!(working_transport.log_count(), 1);
}

#[test]
fn test_transport_flush_failure() {
    let config = MockConfig {
        should_fail_flush: true,
        ..Default::default()
    };
    let transport = MockTransport::with_config(config);

    let logger = Logger::builder().transport(transport).build();

    logger.log(LogInfo::new("info", "Test"));

    // Flush should still complete even if transport fails
    let result = logger.flush();
    assert!(result.is_ok());
}

#[test]
fn test_logging_without_transports_then_adding() {
    let logger = Logger::builder().format(logform::passthrough()).build();

    // Log without transports - should buffer
    logger.log(LogInfo::new("info", "Buffered message 1"));
    logger.log(LogInfo::new("info", "Buffered message 2"));

    // Add transport - should process buffer
    let transport = MockTransport::new();
    logger.add_transport(transport.clone());

    // Log another message
    logger.log(LogInfo::new("info", "Direct message"));
    logger.flush().unwrap();

    // All three messages should be delivered
    assert_eq!(transport.log_count(), 3);
    let logs = transport.get_logs();
    assert_eq!(logs[0].message, "Buffered message 1");
    assert_eq!(logs[1].message, "Buffered message 2");
    assert_eq!(logs[2].message, "Direct message");
}

#[test]
fn test_invalid_log_level() {
    let transport = MockTransport::new();
    let logger = Logger::builder()
        .level("info")
        .transport(transport.clone())
        .build();

    // Log with invalid level - should still be processed
    logger.log(LogInfo::new("nonexistent_level", "Test"));
    logger.flush().unwrap();

    // Message should be filtered out due to invalid level
    assert_eq!(transport.log_count(), 0);
}

#[test]
fn test_empty_message_handling() {
    let transport = MockTransport::new();
    let logger = Logger::builder().transport(transport.clone()).build();

    logger.log(LogInfo::new("info", ""));
    logger.flush().unwrap();

    // Empty messages should be filtered
    assert_eq!(transport.log_count(), 0);
}

#[test]
fn test_large_message() {
    let transport = MockTransport::new();
    let logger = Logger::builder().transport(transport.clone()).build();

    let large_message = "x".repeat(1_000_000); // 1MB message
    logger.log(LogInfo::new("info", &large_message));
    logger.flush().unwrap();

    assert_eq!(transport.log_count(), 1);
    assert!(transport.has_message(&large_message));
}

#[test]
fn test_rapid_configure_calls() {
    let logger = Logger::builder().build();

    // Rapidly reconfigure multiple times
    for i in 0..10 {
        let level = if i % 2 == 0 { "info" } else { "debug" };
        logger.configure(Some(winston::LoggerOptions::new().level(level)));
    }

    // Logger should still be functional
    let transport = MockTransport::new();
    logger.add_transport(transport.clone());
    logger.log(LogInfo::new("info", "After reconfig"));
    logger.flush().unwrap();

    assert!(transport.log_count() > 0);
}

#[test]
fn test_query_with_no_results() {
    let transport = MockTransport::new();
    let logger = Logger::builder().transport(transport).build();

    logger.log(LogInfo::new("info", "Test"));
    logger.flush().unwrap();

    let query = winston::LogQuery::new().levels(vec!["error"]);
    let results = logger.query(&query).unwrap();

    assert_eq!(results.len(), 0);
}

#[test]
fn test_close_then_log() {
    let transport = MockTransport::new();
    let logger = Logger::builder().transport(transport.clone()).build();

    logger.close();

    // Logging after close should not crash (but may not be processed)
    logger.log(LogInfo::new("info", "After close"));

    // This is acceptable behavior - logs after close may be dropped
}

#[test]
fn test_flush_empty_logger() {
    let logger = Logger::builder().build();

    // Flushing empty logger should succeed
    assert!(logger.flush().is_ok());
}

#[test]
fn test_remove_all_transports_then_add() {
    let transport1 = MockTransport::new();
    let transport2 = MockTransport::new();

    let logger = Logger::builder() /*.add_transport(transport1.clone())*/
        .build();
    let transport1_handle = logger.add_transport(transport1.clone());

    logger.remove_transport(transport1_handle);

    // Add new transport after removing all
    logger.add_transport(transport2.clone());

    logger.log(LogInfo::new("info", "Test"));
    logger.flush().unwrap();

    assert_eq!(transport2.log_count(), 1);
}
