mod common;

use common::MockTransport;
use logform::LogInfo;
use serial_test::serial;
use winston::Logger;

#[test]
#[serial]
fn test_global_init() {
    if !winston::is_initialized() {
        let logger = Logger::new(None);
        winston::init(logger);
    }
    assert!(winston::is_initialized());
}

#[test]
#[serial]
#[should_panic(expected = "Global logger already initialized")]
fn test_global_init_twice_panics() {
    if !winston::is_initialized() {
        winston::init(Logger::new(None));
    }
    winston::init(Logger::new(None)); // Should panic
}

#[test]
#[serial]
fn test_global_is_initialized() {
    if !winston::is_initialized() {
        winston::init(Logger::new(None));
    }
    assert!(winston::is_initialized());
}

#[test]
#[serial]
fn test_global_add_transport() {
    if !winston::is_initialized() {
        winston::init(Logger::new(None));
    }

    let transport = MockTransport::new();
    winston::add_transport(transport.clone());

    winston::log(LogInfo::new("info", "Test"));
    winston::flush().unwrap();

    assert_eq!(transport.log_count(), 1);
}

#[test]
#[serial]
fn test_global_remove_transport() {
    if !winston::is_initialized() {
        winston::init(Logger::new(None));
    }

    let transport = MockTransport::new();
    let transport_handle = winston::add_transport(transport.clone());

    assert!(winston::remove_transport(transport_handle));

    winston::log(LogInfo::new("info", "After removal"));
    winston::flush().unwrap();

    assert_eq!(transport.log_count(), 0);
}

#[test]
#[serial]
fn test_global_log() {
    if !winston::is_initialized() {
        winston::init(Logger::new(None));
    }

    let transport = MockTransport::new();
    winston::add_transport(transport.clone());

    winston::log(LogInfo::new("info", "Global message"));
    winston::flush().unwrap();

    assert!(transport.has_message("Global message"));
}

#[test]
#[serial]
fn test_global_try_log_when_initialized() {
    if !winston::is_initialized() {
        winston::init(Logger::new(None));
    }

    let transport = MockTransport::new();
    winston::add_transport(transport.clone());

    assert!(winston::try_log(LogInfo::new("info", "Try log")));
    winston::flush().unwrap();

    assert_eq!(transport.log_count(), 1);
}

/*#[test]
#[serial]
fn test_global_try_log_when_not_initialized() {
    // This test needs to run in a separate process or first
    // For now, skip it or manually ensure logger isn't initialized
    // Since we can't reset OnceLock, this test is problematic with serial tests
    // You might want to remove this test or run it separately
    if !winston::is_initialized() {
        let result = winston::try_log(LogInfo::new("info", "Should fail"));
        assert!(!result);
    }
}*/

#[test]
#[serial]
fn test_global_flush() {
    if !winston::is_initialized() {
        winston::init(Logger::new(None));
    }

    let transport = MockTransport::new();
    winston::add_transport(transport.clone());

    for i in 0..5 {
        winston::log(LogInfo::new("info", &format!("Message {}", i)));
    }

    assert!(winston::flush().is_ok());
    assert_eq!(transport.log_count(), 5);
}

#[test]
#[serial]
fn test_global_query() {
    if !winston::is_initialized() {
        winston::init(Logger::builder().format(logform::timestamp()).build());
    } else {
        // Reconfigure to ensure timestamp format is present
        winston::configure(Some(
            winston::LoggerOptions::new().format(logform::timestamp()),
        ));
    }

    let transport = MockTransport::new();
    winston::add_transport(transport.clone());

    winston::log(LogInfo::new("info", "Info log"));
    winston::log(LogInfo::new("error", "Error log"));
    winston::flush().unwrap();

    let query = winston::LogQuery::new().levels(vec!["error"]);
    let results = winston::query(&query).unwrap();

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].level, "error");
}

// Causes inconsistency in other tests since transports is cleared on configure
/*#[test]
#[serial]
fn test_global_configure() {
    if !winston::is_initialized() {
        winston::init(Logger::new(None));
    }

     let transport = MockTransport::new();

    winston::configure(Some(
        winston::LoggerOptions::new()
            .level("error")
            .add_transport(transport.clone()),
    ));

    winston::log(LogInfo::new("info", "Filtered"));
    winston::log(LogInfo::new("error", "Passes"));
    winston::flush().unwrap();

    assert_eq!(transport.log_count(), 1);
}*/

// Once closed, the logger can't be used again. But all global tests share the same global logger instance,
// so after test_global_close runs, every subsequent test has a dead logger.
/*#[test]
#[serial]
fn test_global_close() {
    if !winston::is_initialized() {
        winston::init(Logger::new(None));
    }

     let transport = MockTransport::new();
    winston::add_transport(transport.clone());

    winston::log(LogInfo::new("info", "Before close"));
    winston::close();

    assert_eq!(transport.log_count(), 1);
}*/

// These two tests are problematic with serial execution since logger stays initialized
// Consider removing them or running them in a separate test binary
/*
#[test]
#[should_panic(expected = "Global logger not initialized")]
fn test_global_log_without_init_panics() {
    winston::log(LogInfo::new("info", "Should panic"));
}

#[test]
#[should_panic(expected = "Global logger not initialized")]
fn test_global_flush_without_init_panics() {
    let _ = winston::flush();
}
*/
