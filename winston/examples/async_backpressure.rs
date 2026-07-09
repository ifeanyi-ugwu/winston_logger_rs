//! Cooperative async backpressure with `log_async`.
//!
//! Run: `cargo run -p winston --example async_backpressure --features async-log`
//!
//! A `Block` transport with a small mailbox and a deliberately slow sink forces
//! backpressure. Under sync `log()`, a saturated `Block` slot parks the calling
//! thread — on an async runtime that means parking a worker (and, if the pump
//! shared a single-threaded runtime, deadlocking it). `log_async` instead
//! **yields the task** while it waits for mailbox room, so the worker stays free
//! and many loggers make progress cooperatively.
//!
//! This runs on a single-threaded runtime with eight concurrent loggers and a
//! sink that can only drain one line every 20 ms: the mailbox saturates, the
//! loggers cooperatively await room, and all forty lines are delivered — none
//! dropped, no deadlock.

use std::sync::Arc;
use std::time::Duration;

use winston::format::{FormattedEntry, LogInfo};
use winston::{Logger, LoggerTransport, OverflowPolicy, Transport, TransportResult};

// A deliberately slow sink. The blocking sleep simulates a slow writer; it runs
// on the transport's own pump thread (the default spawner), so it throttles the
// mailbox without needing an async timer or a runtime context.
struct SlowSink;

impl Transport for SlowSink {
    async fn log(&mut self, entry: FormattedEntry) -> TransportResult<()> {
        std::thread::sleep(Duration::from_millis(20));
        println!("  sink wrote: {}", entry.info.message);
        Ok(())
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let logger = Arc::new(
        Logger::builder()
            .transport(
                LoggerTransport::new(SlowSink)
                    .with_queue_capacity(4)
                    .with_overflow_policy(OverflowPolicy::Block),
            )
            .build(),
    );

    // Eight concurrent tasks, each logging five must-not-drop lines. With the
    // sink slow and the mailbox capped at 4, most calls await room — the tasks
    // yield cooperatively rather than parking the (single) runtime worker.
    let mut tasks = Vec::new();
    for t in 0..8 {
        let logger = Arc::clone(&logger);
        tasks.push(tokio::spawn(async move {
            for i in 0..5 {
                logger
                    .log_async(LogInfo::new("info", format!("task {t} · line {i}")))
                    .await;
            }
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }

    // flush() is synchronous — run it off the async worker (via spawn_blocking)
    // so it doesn't block the runtime while the pump drains. This is the same
    // sync-teardown caveat that log_async exists to avoid on the hot path.
    let l = Arc::clone(&logger);
    tokio::task::spawn_blocking(move || l.flush().unwrap())
        .await
        .unwrap();

    println!("\nall 40 lines logged, none dropped — the backpressure was cooperative");
}
