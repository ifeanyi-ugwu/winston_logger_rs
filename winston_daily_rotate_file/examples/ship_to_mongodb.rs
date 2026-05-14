//! The canonical exactly-once-ish proxy recipe, end to end:
//!
//!   DailyRotateFile (live, crash-recoverable source)
//!     → archive::ship_rotated_files
//!       → MongoDBTransport::idempotent_ingest_handle (keyed on content_id)
//!
//! A crash anywhere in this pipeline is safe: the rotated file survives until
//! it's been fully shipped *and* deleted, so the next run re-ships it; the
//! idempotent Mongo handle keys each doc on its `content_id`, so the re-ship
//! collides on `_id` and is a no-op — the collection converges with no net
//! duplicates.
//!
//! Run with a MongoDB reachable at `$MONGODB_URI`:
//!
//! ```text
//! MONGODB_URI=mongodb://localhost:27017 \
//!   cargo run -p winston_daily_rotate_file --example ship_to_mongodb
//! ```
//!
//! Without `MONGODB_URI` set, the example prints a note and exits 0.

use std::env;
use std::pin::Pin;
use std::time::Duration;

use logform::{json, LogInfo};
use winston::{Logger, LoggerOptions};
use winston_daily_rotate_file::{archive::ship_rotated_files, DailyRotateFile};
use winston_mongodb::{MongoDBOptions, MongoDBTransport};
use winston_transport::DynIngestHandle;

#[tokio::main]
async fn main() {
    let Ok(uri) = env::var("MONGODB_URI") else {
        eprintln!("MONGODB_URI not set — nothing to ship to. Set it and re-run:");
        eprintln!("  MONGODB_URI=mongodb://localhost:27017 cargo run -p winston_daily_rotate_file --example ship_to_mongodb");
        return;
    };

    let tmp = tempfile::tempdir().expect("temp dir");
    let log_path = tmp.path().join("app.log");

    // Tiny `max_size` so a handful of JSON lines forces several rotations,
    // giving us real rotated files to ship. In production you'd rotate by
    // date (or a sane size) and ship on a timer.
    let drf = DailyRotateFile::builder()
        .filename(&log_path)
        .date_pattern("%Y-%m-%d")
        .max_size(200)
        .build()
        .expect("build DailyRotateFile");

    // Pull the rotation handle off BEFORE the Logger consumes the transport.
    let rotation = drf.rotation_handle();

    // MongoDB needs a tokio reactor; the Logger's per-transport WritableStream
    // tasks must run inside one. (DailyRotate itself doesn't need tokio, but
    // using a single tokio spawner for the whole Logger keeps things simple
    // when a tokio transport is in the mix.)
    let logger = Logger::new_with_spawner(
        Some(LoggerOptions::new().level("trace").format(json())),
        winston_mongodb::tokio_spawner(),
    );
    logger.add_transport(drf);

    // The Logger's global `json()` format serializes each entry to a JSON
    // line — that's what makes the rotated files round-trip through
    // `FileSource` when they're shipped.
    for i in 0..20 {
        logger.log(LogInfo::new("info", &format!("event {i}")));
    }
    logger.flush().expect("flush");

    let mongo = MongoDBTransport::new(MongoDBOptions {
        connection_string: uri.clone(),
        database: "winston_proxy_example".to_string(),
        collection: "shipped_logs".to_string(),
    });
    let mongo_ingest: Box<dyn DynIngestHandle> = mongo.idempotent_ingest_handle();

    // The spawner `ship_rotated_files` uses to drive each file's read stream.
    let spawn = |fut: Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>| {
        tokio::spawn(fut);
    };

    let stats1 = ship_rotated_files(&rotation, &*mongo_ingest, 50, spawn)
        .await
        .expect("ship pass 1");
    println!(
        "pass 1: shipped {} entries across {} files ({} ship-fail, {} delete-fail)",
        stats1.entries_shipped,
        stats1.files_shipped,
        stats1.ship_failures,
        stats1.delete_failures,
    );

    let stats2 = ship_rotated_files(&rotation, &*mongo_ingest, 50, spawn)
        .await
        .expect("ship pass 2");
    println!(
        "pass 2: shipped {} entries (expected 0 — pass 1 deleted the rotated files)",
        stats2.entries_shipped,
    );

    // Re-ingest the same batch three times directly — the `_id` collision
    // means the collection ends up with exactly one copy of each entry.
    let dup_batch = vec![
        LogInfo::new("warn", "idempotency-demo a"),
        LogInfo::new("warn", "idempotency-demo b"),
    ];
    for _ in 0..3 {
        mongo_ingest.ingest(dup_batch.clone()).await.expect("ingest");
    }
    println!("re-ingested a 2-entry batch 3× via idempotent_ingest_handle — collection has 2 copies, not 6 (verify in Mongo)");

    // Give the background WritableStream tasks a moment to drain, then exit.
    tokio::time::sleep(Duration::from_millis(200)).await;
    logger.close();
    println!("done — log dir was {}", tmp.path().display());
}
