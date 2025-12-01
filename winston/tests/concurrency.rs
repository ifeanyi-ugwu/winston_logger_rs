mod common;

use common::MockTransport;
use logform::LogInfo;
use std::sync::{Arc, Barrier};
use std::thread;
use winston::Logger;

#[test]
fn test_concurrent_logging() {
    let transport = MockTransport::new();
    let logger = Arc::new(Logger::builder().transport(transport.clone()).build());

    let num_threads = 10;
    let messages_per_thread = 100;
    let barrier = Arc::new(Barrier::new(num_threads));

    let handles: Vec<_> = (0..num_threads)
        .map(|thread_id| {
            let logger = Arc::clone(&logger);
            let barrier = Arc::clone(&barrier);

            thread::spawn(move || {
                // Wait for all threads to be ready
                barrier.wait();

                for i in 0..messages_per_thread {
                    logger.log(LogInfo::new(
                        "info",
                        &format!("Thread {} - Message {}", thread_id, i),
                    ));
                }
            })
        })
        .collect();

    for handle in handles {
        handle.join().unwrap();
    }

    logger.flush().unwrap();

    assert_eq!(
        transport.log_count(),
        num_threads * messages_per_thread,
        "All messages should be logged"
    );
}

#[test]
fn test_concurrent_add_remove_transport() {
    let logger = Arc::new(Logger::builder().build());
    let num_threads = 5;

    let handles: Vec<_> = (0..num_threads)
        .map(|_| {
            let logger = Arc::clone(&logger);

            thread::spawn(move || {
                let transport = MockTransport::new();

                // Add transport
                let transport_handle = logger.add_transport(transport.clone());

                // Log some messages
                for i in 0..10 {
                    logger.log(LogInfo::new("info", &format!("Message {}", i)));
                }

                logger.flush().unwrap();

                // Remove transport
                logger.remove_transport(transport_handle);
            })
        })
        .collect();

    for handle in handles {
        handle.join().unwrap();
    }

    // Logger should still be functional
    let final_transport = MockTransport::new();
    logger.add_transport(final_transport.clone());
    logger.log(LogInfo::new("info", "Final message"));
    logger.flush().unwrap();

    assert_eq!(final_transport.log_count(), 1);
}

#[test]
fn test_concurrent_configure() {
    let logger = Arc::new(Logger::builder().build());
    let transport = MockTransport::new();
    //logger.add_transport(transport.clone());

    let num_threads = 5;
    let handles: Vec<_> = (0..num_threads)
        .map(|thread_id| {
            let logger = Arc::clone(&logger);

            thread::spawn(move || {
                // Reconfigure
                logger.configure(Some(winston::LoggerOptions::new().level("debug")));

                // Log message
                logger.log(LogInfo::new("debug", &format!("Thread {}", thread_id)));
            })
        })
        .collect();

    for handle in handles {
        handle.join().unwrap();
    }

    logger.add_transport(transport.clone());

    logger.flush().unwrap();

    // Logger should still work after concurrent reconfiguration
    assert!(transport.log_count() > 0);
}

#[test]
fn test_logging_non_blocking() {
    let transport = MockTransport::with_delay(std::time::Duration::from_millis(100));
    let logger = Logger::builder().transport(transport.clone()).build();

    let num_messages = 10;
    let start = std::time::Instant::now();

    for i in 0..num_messages {
        logger.log(LogInfo::new("info", &format!("Message {}", i)));
    }

    let enqueue_time = start.elapsed();

    // Enqueueing should be much faster than synchronous processing would be
    let synchronous_time = std::time::Duration::from_millis(100) * num_messages;
    assert!(
        enqueue_time < synchronous_time / 5,
        "Enqueueing should be non-blocking and fast"
    );

    logger.flush().unwrap();
    assert_eq!(transport.log_count(), num_messages as usize);
}

#[test]
fn test_flush_waits_for_processing() {
    let transport = MockTransport::with_delay(std::time::Duration::from_millis(50));
    let logger = Logger::builder().transport(transport.clone()).build();

    for i in 0..5 {
        logger.log(LogInfo::new("info", &format!("Message {}", i)));
    }

    // Before flush, logs might not be processed yet
    let count_before = transport.log_count();

    logger.flush().unwrap();

    // After flush, all logs should be processed
    assert_eq!(transport.log_count(), 5);
    assert!(transport.log_count() >= count_before);
}

#[test]
fn test_close_from_multiple_threads() {
    let logger = Arc::new(Logger::builder().transport(MockTransport::new()).build());

    let handles: Vec<_> = (0..3)
        .map(|_| {
            let logger = Arc::clone(&logger);
            thread::spawn(move || {
                logger.close();
            })
        })
        .collect();

    for handle in handles {
        handle.join().unwrap();
    }

    // Logger should handle concurrent close calls gracefully
}

#[test]
fn test_concurrent_query() {
    let transport = MockTransport::new();
    let logger = Arc::new(
        Logger::builder()
            .format(logform::timestamp())
            .transport(transport)
            .build(),
    );

    // Log some messages
    for i in 0..20 {
        logger.log(
            LogInfo::new("info", &format!("Message {}", i)), //.with_meta("timestamp", chrono::Utc::now().to_rfc3339()),
        );
    }
    logger.flush().unwrap();

    // Query from multiple threads
    let handles: Vec<_> = (0..5)
        .map(|_| {
            let logger = Arc::clone(&logger);
            thread::spawn(move || {
                let query = winston::LogQuery::new();
                logger.query(&query).unwrap()
            })
        })
        .collect();

    for handle in handles {
        let results = handle.join().unwrap();
        assert_eq!(results.len(), 20);
    }
}
