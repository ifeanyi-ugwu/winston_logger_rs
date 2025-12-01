mod common;

use common::MockTransport;
use serial_test::serial;
use winston::{log, meta, Logger};

#[test]
fn test_log_macro_with_logger_simple_message() {
    let transport = MockTransport::new();
    let logger = Logger::builder().transport(transport.clone()).build();

    log!(logger, info, "Simple message");
    logger.flush().unwrap();

    assert_eq!(transport.log_count(), 1);
    let logs = transport.get_logs();
    assert_eq!(logs[0].level, "info");
    assert!(logs[0].message.contains("Simple message"));
}

#[test]
fn test_log_macro_with_logger_and_metadata() {
    let transport = MockTransport::new();
    let logger = Logger::builder()
        .format(logform::passthrough())
        .transport(transport.clone())
        .build();

    log!(logger, warn, "Warning message", key1 = "value1", key2 = 42);
    logger.flush().unwrap();

    let logs = transport.get_logs();
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0].level, "warn");
    assert!(logs[0].meta.contains_key("key1"));
    assert!(logs[0].meta.contains_key("key2"));
}

#[test]
fn test_log_macro_with_logger_and_meta_macro() {
    let transport = MockTransport::new();
    let logger = Logger::builder()
        .format(logform::passthrough())
        .transport(transport.clone())
        .build();

    log!(
        logger,
        error,
        "Error with meta",
        meta!(user_id = 123, action = "delete")
    );
    logger.flush().unwrap();

    let logs = transport.get_logs();
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0].level, "error");
    assert!(logs[0].meta.contains_key("user_id"));
    assert!(logs[0].meta.contains_key("action"));
}

#[test]
fn test_log_macro_with_format_string() {
    let transport = MockTransport::new();
    let logger = Logger::builder().transport(transport.clone()).build();

    let user = "Alice";
    let count = 5;
    log!(logger, info, format!("User {} has {} items", user, count));
    logger.flush().unwrap();

    assert!(transport.has_message("User Alice has 5 items"));
}

#[test]
fn test_meta_macro_creates_vec() {
    let metadata = meta!(id = 5, name = "test", active = true);

    assert_eq!(metadata.len(), 3);
    assert_eq!(metadata[0].0, "id");
    assert_eq!(metadata[1].0, "name");
    assert_eq!(metadata[2].0, "active");
}

#[test]
fn test_meta_macro_with_different_types() {
    let metadata = meta!(
        string_val = "text",
        int_val = 42,
        float_val = 3.14,
        bool_val = true
    );

    assert_eq!(metadata.len(), 4);
}

#[test]
fn test_log_macro_different_levels() {
    let transport = MockTransport::new();
    let logger = Logger::builder()
        .level("trace")
        .transport(transport.clone())
        .build();

    log!(logger, trace, "Trace message");
    log!(logger, debug, "Debug message");
    log!(logger, info, "Info message");
    log!(logger, warn, "Warn message");
    log!(logger, error, "Error message");
    logger.flush().unwrap();

    assert_eq!(transport.log_count(), 5);
    let logs = transport.get_logs();
    assert_eq!(logs[0].level, "trace");
    assert_eq!(logs[1].level, "debug");
    assert_eq!(logs[2].level, "info");
    assert_eq!(logs[3].level, "warn");
    assert_eq!(logs[4].level, "error");
}

#[test]
fn test_log_macro_with_trailing_comma() {
    let transport = MockTransport::new();
    let logger = Logger::builder().transport(transport.clone()).build();

    log!(logger, info, "Message", key = "value",);
    logger.flush().unwrap();

    assert_eq!(transport.log_count(), 1);
}

#[test]
fn test_log_macro_with_complex_metadata_values() {
    let transport = MockTransport::new();
    let logger = Logger::builder()
        .format(logform::passthrough())
        .transport(transport.clone())
        .build();

    let nested_value = serde_json::json!({
        "nested": {
            "field": "value"
        }
    });

    log!(logger, info, "Complex metadata", data = nested_value);
    logger.flush().unwrap();

    let logs = transport.get_logs();
    assert!(logs[0].meta.contains_key("data"));
}

// Tests for global logger with macros
#[test]
#[serial]
fn test_log_macro_with_global_logger() {
    let transport = MockTransport::new();

    if !winston::is_initialized() {
        winston::init(Logger::builder().build());
    }
    winston::add_transport(transport.clone());

    log!(info, "Global message");
    winston::flush().unwrap();

    assert_eq!(transport.log_count(), 1);

    //winston::close();
}

#[test]
#[serial]
fn test_log_macro_with_global_and_metadata() {
    let transport = MockTransport::new();

    if !winston::is_initialized() {
        winston::init(Logger::builder().format(logform::passthrough()).build());
    } else {
        // Reconfigure to ensure passthrough format is present
        winston::configure(Some(
            winston::LoggerOptions::new().format(logform::passthrough()),
        ));
    }
    winston::add_transport(transport.clone());

    log!(warn, "Global warning", code = 404, reason = "not found");
    winston::flush().unwrap();

    let logs = transport.get_logs();
    assert_eq!(logs.len(), 1);
    assert!(logs[0].meta.contains_key("code"));
    assert!(logs[0].meta.contains_key("reason"));

    //winston::close();
}

#[test]
#[serial]
fn test_log_macro_with_global_and_meta_macro() {
    let transport = MockTransport::new();

    if !winston::is_initialized() {
        winston::init(Logger::builder().format(logform::passthrough()).build());
    } else {
        // Reconfigure to ensure passthrough format is present
        winston::configure(Some(
            winston::LoggerOptions::new().format(logform::passthrough()),
        ));
    }
    winston::add_transport(transport.clone());

    log!(
        error,
        "Global error",
        meta!(severity = "high", retry = false)
    );
    winston::flush().unwrap();

    let logs = transport.get_logs();
    assert_eq!(logs.len(), 1);
    assert!(logs[0].meta.contains_key("severity"));

    ////winston::close();
}
