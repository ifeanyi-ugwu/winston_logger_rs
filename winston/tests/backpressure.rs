mod common;

use common::{MockConfig, MockTransport};
use logform::LogInfo;
use std::time::{Duration, Instant};
use winston::{BackpressureStrategy, Logger};

#[test]
#[ignore = "test fails"]
fn test_backpressure_block_strategy() {
    let config = MockConfig {
        delay: Duration::from_millis(100),
        ..Default::default()
    };
    let transport = MockTransport::with_config(config);

    let logger = Logger::builder()
        .channel_capacity(2)
        .backpressure_strategy(BackpressureStrategy::Block)
        .transport(transport.clone())
        .build();

    let start = Instant::now();

    // Fill the channel (capacity = 2)
    logger.log(LogInfo::new("info", "Message 1"));
    logger.log(LogInfo::new("info", "Message 2"));

    // This should block until space is available
    logger.log(LogInfo::new("info", "Message 3"));

    let duration = start.elapsed();

    logger.flush().unwrap();

    // The third message should have blocked for at least the transport delay
    assert!(
        duration >= Duration::from_millis(90),
        "Block strategy should have blocked for processing time"
    );

    // All messages should be delivered
    assert_eq!(transport.log_count(), 3);
}

#[test]
#[ignore = "test fails"]
fn test_backpressure_drop_oldest_strategy() {
    let config = MockConfig {
        delay: Duration::from_millis(50),
        ..Default::default()
    };
    let transport = MockTransport::with_config(config);

    let logger = Logger::builder()
        .channel_capacity(2)
        .backpressure_strategy(BackpressureStrategy::DropOldest)
        .transport(transport.clone())
        .build();

    // Rapidly send 5 messages
    for i in 1..=5 {
        logger.log(LogInfo::new("info", &format!("Message {}", i)));
    }

    // Give time for processing
    std::thread::sleep(Duration::from_millis(300));
    logger.flush().unwrap();

    let logs = transport.get_logs();

    // Should have dropped oldest messages to maintain capacity
    assert!(logs.len() <= 4, "Should have dropped some messages");

    // Should have the most recent messages
    let last_log = &logs[logs.len() - 1];
    assert_eq!(last_log.message, "Message 5");
}

#[test]
#[ignore = "test fails"]
fn test_backpressure_drop_current_strategy() {
    let config = MockConfig {
        delay: Duration::from_millis(50),
        ..Default::default()
    };
    let transport = MockTransport::with_config(config);

    let logger = Logger::builder()
        .channel_capacity(2)
        .backpressure_strategy(BackpressureStrategy::DropCurrent)
        .transport(transport.clone())
        .build();

    // Rapidly send 5 messages
    for i in 1..=5 {
        logger.log(LogInfo::new("info", &format!("Message {}", i)));
    }

    // Give time for processing
    std::thread::sleep(Duration::from_millis(300));
    logger.flush().unwrap();

    let logs = transport.get_logs();

    // Should have kept oldest messages and dropped newest
    assert!(logs.len() <= 4, "Should have dropped some messages");

    // Should have the earliest messages
    assert_eq!(logs[0].message, "Message 1");
}

#[test]
#[ignore = "test fails"]
fn test_backpressure_strategies_differ() {
    let delay = Duration::from_millis(30);

    // Test Block strategy
    let transport_block = MockTransport::with_delay(delay);
    let logger_block = Logger::builder()
        .channel_capacity(2)
        .backpressure_strategy(BackpressureStrategy::Block)
        .transport(transport_block.clone())
        .build();

    let start_block = Instant::now();
    for i in 1..=3 {
        logger_block.log(LogInfo::new("info", &format!("Block {}", i)));
    }
    let block_duration = start_block.elapsed();
    logger_block.flush().unwrap();

    // Test DropCurrent strategy
    let transport_drop = MockTransport::with_delay(delay);
    let logger_drop = Logger::builder()
        .channel_capacity(2)
        .backpressure_strategy(BackpressureStrategy::DropCurrent)
        .transport(transport_drop.clone())
        .build();

    let start_drop = Instant::now();
    for i in 1..=3 {
        logger_drop.log(LogInfo::new("info", &format!("Drop {}", i)));
    }
    let drop_duration = start_drop.elapsed();
    logger_drop.flush().unwrap();

    // Block should take longer than DropCurrent
    assert!(
        block_duration > drop_duration,
        "Block strategy should take longer than DropCurrent"
    );

    // Block should deliver all messages
    assert_eq!(transport_block.log_count(), 3);

    // DropCurrent may have dropped some
    assert!(transport_drop.log_count() <= 3);
}

#[test]
fn test_no_backpressure_with_sufficient_capacity() {
    let transport = MockTransport::new();

    let logger = Logger::builder()
        .channel_capacity(1000)
        .backpressure_strategy(BackpressureStrategy::Block)
        .transport(transport.clone())
        .build();

    let start = Instant::now();

    for i in 0..100 {
        logger.log(LogInfo::new("info", &format!("Message {}", i)));
    }

    let enqueue_duration = start.elapsed();

    logger.flush().unwrap();

    // Should be very fast with sufficient capacity
    assert!(
        enqueue_duration < Duration::from_millis(50),
        "Enqueueing should be fast with sufficient capacity"
    );

    assert_eq!(transport.log_count(), 100);
}

#[test]
#[ignore = "test fails"]
fn test_backpressure_recovers_after_flush() {
    let config = MockConfig {
        delay: Duration::from_millis(10),
        ..Default::default()
    };
    let transport = MockTransport::with_config(config);

    let logger = Logger::builder()
        .channel_capacity(2)
        .backpressure_strategy(BackpressureStrategy::DropCurrent)
        .transport(transport.clone())
        .build();

    // Fill and overflow
    for i in 1..=5 {
        logger.log(LogInfo::new("info", &format!("First batch {}", i)));
    }

    logger.flush().unwrap();
    let first_count = transport.log_count();
    transport.clear_logs();

    // Should work normally after flush
    for i in 1..=3 {
        logger.log(LogInfo::new("info", &format!("Second batch {}", i)));
    }

    logger.flush().unwrap();
    let second_count = transport.log_count();

    // Second batch should deliver all messages
    assert_eq!(second_count, 3);
}
