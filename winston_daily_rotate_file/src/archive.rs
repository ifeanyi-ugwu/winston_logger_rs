//! Helpers for shipping rotated log files into a remote target.
//!
//! The legacy `Proxy<T>` pattern's atomic-rotate-and-archive role is split
//! across three primitives in the new architecture:
//!
//! 1. [`crate::DailyRotateRotationHandle::list_rotated_files`] — observe
//!    which files are no longer active and safe to read.
//! 2. [`winston_file::FileSource`] — open one of those files as a streaming
//!    source of `LogInfo`.
//! 3. [`winston_transport::DynIngestHandle`] — pump those entries into
//!    a target transport (HTTP, MongoDB, …) as batches.
//!
//! [`ship_rotated_files`] composes all three: list, drain each file into the
//! target, then delete the file on success. Run it on a timer for the legacy
//! `ProxyTransport`'s "every N seconds" behavior.

use std::{
    fs::File,
    future::Future,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    pin::Pin,
};

use flate2::read::GzDecoder;
use winston_file::FileSource;
use winston_transport::{
    proxy::pipe_to_ingest, DynIngestHandle, DynReadableSource, LogQuery,
};

use crate::DailyRotateRotationHandle;

/// One pass: list rotated files, drain each into `target`, delete on success.
///
/// `spawn_fn` is the task spawner used to drive each file's `ReadableStream`
/// (same shape as the one you pass to a Logger). `batch_size` controls the
/// per-`ingest` call size.
///
/// On a per-file failure, the file is left in place — no delete. The function
/// continues with the remaining files. Counts of successes and failures are
/// surfaced via [`ShipStats`]. Failed files will be retried on the next call,
/// which is the right behavior for periodic shipping.
///
/// # Recommended periodic-shipping pattern
///
/// `ship_rotated_files` is one pass. For "every N seconds, ship whatever has
/// rotated since last time," wrap it in your own loop with whatever timer
/// fits your runtime. The shape is intentionally not packaged here —
/// runtime choice, retry policy, shutdown signal, and observability are all
/// caller-specific decisions that a one-size helper would prejudge.
///
/// ```ignore
/// use std::time::Duration;
/// use winston_daily_rotate_file::archive::ship_rotated_files;
///
/// // `rotation`: DailyRotateRotationHandle, `target`: &dyn DynIngestHandle,
/// // `spawn_fn`: your task spawner (e.g. winston::default_spawner()).
/// loop {
///     match ship_rotated_files(&rotation, target, 100, spawn_fn.clone()).await {
///         Ok(stats) => {
///             if stats.ship_failures > 0 || stats.delete_failures > 0 {
///                 eprintln!(
///                     "shipping: {} ok / {} ship-fail / {} delete-fail: {:?}",
///                     stats.files_shipped,
///                     stats.ship_failures,
///                     stats.delete_failures,
///                     stats.last_ship_error.as_deref().or(stats.last_delete_error.as_deref()),
///                 );
///             }
///         }
///         Err(e) => eprintln!("listing rotated files failed: {e}"),
///     }
///     tokio::time::sleep(Duration::from_secs(60)).await;
///     // ...or std::thread::sleep, or whatever timer your runtime uses.
/// }
/// ```
pub async fn ship_rotated_files<F>(
    rotation: &DailyRotateRotationHandle,
    target: &dyn DynIngestHandle,
    batch_size: usize,
    spawn_fn: F,
) -> std::io::Result<ShipStats>
where
    F: Fn(Pin<Box<dyn Future<Output = ()> + Send + 'static>>) + Send + Sync + Clone + 'static,
{
    let files = rotation.list_rotated_files()?;
    let mut stats = ShipStats::default();

    for path in files {
        match ship_one(&path, target, batch_size, spawn_fn.clone()).await {
            Ok(n) => {
                stats.entries_shipped += n;
                match std::fs::remove_file(&path) {
                    Ok(()) => stats.files_shipped += 1,
                    Err(e) => {
                        // Entries are at the target but we couldn't delete the
                        // local file — next pass will reship them. Surface as
                        // a delete failure so callers know.
                        stats.delete_failures += 1;
                        stats.last_delete_error = Some(format!("{}: {e}", path.display()));
                    }
                }
            }
            Err(e) => {
                stats.ship_failures += 1;
                stats.last_ship_error = Some(format!("{}: {e}", path.display()));
            }
        }
    }

    Ok(stats)
}

async fn ship_one<F>(
    path: &Path,
    target: &dyn DynIngestHandle,
    batch_size: usize,
    spawn_fn: F,
) -> Result<usize, ShipError>
where
    F: Fn(Pin<Box<dyn Future<Output = ()> + Send + 'static>>) + Send + Sync + 'static,
{
    // `zipped_archive` rotated files arrive here as `.gz` — decompress on the
    // fly so they ship (and then get deleted) just like plain files. Anything
    // else is read as a plain JSON-lines file.
    let file = File::open(path).map_err(ShipError::Open)?;
    let reader: Box<dyn BufRead + Send> =
        if path.extension().and_then(|e| e.to_str()) == Some("gz") {
            Box::new(BufReader::new(GzDecoder::new(file)))
        } else {
            Box::new(BufReader::new(file))
        };
    let source: Box<dyn DynReadableSource> =
        Box::new(FileSource::from_reader(reader, LogQuery::new()));
    let receipt = pipe_to_ingest(source, target, batch_size, move |fut| spawn_fn(fut))
        .await
        .map_err(|e| ShipError::Pipe(e.to_string()))?;
    Ok(receipt.entries_shipped())
}

/// Outcome of one `ship_rotated_files` pass. Counters are independent — a
/// pass might successfully ship 3 files, hit a network failure on 1, and
/// fail to delete 1 of the successfully shipped ones.
#[derive(Debug, Default, Clone)]
pub struct ShipStats {
    /// Files for which both the ingest and the delete succeeded.
    pub files_shipped: usize,
    /// Total entries shipped across all successful files.
    pub entries_shipped: usize,
    /// Files where the ingest itself failed; the file is left in place.
    pub ship_failures: usize,
    /// Files that ingested cleanly but couldn't be deleted afterward; the
    /// next pass will reship them.
    pub delete_failures: usize,
    pub last_ship_error: Option<String>,
    pub last_delete_error: Option<String>,
}

#[derive(Debug)]
enum ShipError {
    Open(std::io::Error),
    Pipe(String),
}

impl std::fmt::Display for ShipError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ShipError::Open(e) => write!(f, "open: {e}"),
            ShipError::Pipe(e) => write!(f, "pipe: {e}"),
        }
    }
}

/// Convenience reference to the rotated-files PathBuf list, in case a caller
/// wants to inspect what would be shipped without actually doing it.
#[doc(hidden)]
pub fn list_rotated_for_test(rotation: &DailyRotateRotationHandle) -> std::io::Result<Vec<PathBuf>> {
    rotation.list_rotated_files()
}

#[cfg(test)]
mod tests {
    use super::*;
    use logform::{json, timestamp, Format, LogInfo};
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;
    use whatwg_streams::{CountQueuingStrategy, StreamResult, WritableStream};
    use winston_transport::DynIngestHandle;

    use crate::DailyRotateFile;

    /// Captures every batch passed to `ingest`.
    #[derive(Clone)]
    struct CaptureIngest(Arc<Mutex<Vec<LogInfo>>>);

    impl DynIngestHandle for CaptureIngest {
        fn ingest<'s>(
            &'s self,
            logs: Vec<LogInfo>,
        ) -> Pin<Box<dyn Future<Output = StreamResult<()>> + Send + 's>> {
            let store = Arc::clone(&self.0);
            Box::pin(async move {
                store.lock().unwrap().extend(logs);
                Ok(())
            })
        }
    }

    fn json_log(level: &str, msg: &str) -> LogInfo {
        json()
            .transform(timestamp().transform(LogInfo::new(level, msg)).unwrap())
            .unwrap()
    }

    fn thread_spawner(fut: Pin<Box<dyn Future<Output = ()> + Send + 'static>>) {
        std::thread::spawn(move || futures::executor::block_on(fut));
    }

    /// Drives the transport through a real WritableStream, runs the closure
    /// against the writer, then closes so rotation/flush completes.
    fn drive<F, Fut>(transport: DailyRotateFile, body: F)
    where
        F: FnOnce(
            whatwg_streams::WritableStreamDefaultWriter<LogInfo, DailyRotateFile>,
        ) -> Fut,
        Fut: Future<Output = ()>,
    {
        // Re-borrow the spawner because Fn-not-FnOnce traverses ownership.
        let stream = WritableStream::builder(transport)
            .strategy(CountQueuingStrategy::new(64))
            .spawn(|fut| {
                std::thread::spawn(move || futures::executor::block_on(fut));
            });
        futures::executor::block_on(async {
            let (_locked, writer) = stream.get_writer().expect("get_writer");
            body(writer).await;
        });
    }

    /// End-to-end: drive several rotations, ship the rotated files into a
    /// captured ingest, verify (a) entries arrived in order, (b) the
    /// rotated files are gone, (c) the active file is untouched.
    #[test]
    fn ship_rotated_files_drains_and_deletes() {
        let temp_dir = TempDir::new().expect("temp dir");
        let log_path = temp_dir.path().join("ship.log");

        let transport = DailyRotateFile::builder()
            .filename(&log_path)
            .date_pattern("%Y-%m-%d_%H-%M-%S")
            .build()
            .expect("build transport");
        let rotation = transport.rotation_handle();

        // Force two rotations by writing across one-second boundaries.
        drive(transport, |writer| async move {
            writer.write(json_log("info", "first")).await.expect("write 1");
            std::thread::sleep(std::time::Duration::from_secs(1));
            writer.write(json_log("info", "second")).await.expect("write 2");
            std::thread::sleep(std::time::Duration::from_secs(1));
            writer.write(json_log("info", "third")).await.expect("write 3");
            writer.close().await.expect("close");
        });

        // After close, three log files exist; two are "rotated".
        let captured = Arc::new(Mutex::new(Vec::<LogInfo>::new()));
        let target = CaptureIngest(Arc::clone(&captured));

        let stats = futures::executor::block_on(ship_rotated_files(
            &rotation,
            &target,
            /*batch_size*/ 10,
            thread_spawner,
        ))
        .expect("ship_rotated_files");

        assert_eq!(stats.files_shipped, 2, "expected 2 rotated files shipped");
        assert_eq!(stats.entries_shipped, 2, "one entry per rotated file");
        assert_eq!(stats.ship_failures, 0);
        assert_eq!(stats.delete_failures, 0);

        // Captured entries match.
        let entries = captured.lock().unwrap();
        let messages: Vec<String> = entries.iter().map(|e| e.message.clone()).collect();
        assert!(messages.contains(&"first".to_string()));
        assert!(messages.contains(&"second".to_string()));
        assert!(!messages.contains(&"third".to_string()));

        // Active file remains; rotated files are gone.
        let remaining: Vec<_> = std::fs::read_dir(temp_dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_file())
            .collect();
        assert_eq!(
            remaining.len(),
            1,
            "only the active file should remain, found {}",
            remaining.len()
        );
    }

    /// With `zipped_archive`, rotated files arrive as `.gz`. `ship_rotated_files`
    /// must decompress, ship, and delete them — not mangle or skip.
    #[test]
    fn ship_rotated_files_handles_gzipped_files() {
        let temp_dir = TempDir::new().expect("temp dir");
        let log_path = temp_dir.path().join("gz.log");

        let transport = DailyRotateFile::builder()
            .filename(&log_path)
            .date_pattern("%Y-%m-%d")
            .max_size(100) // small enough that JSON lines trigger rotation fast
            .zipped_archive(true)
            .build()
            .expect("build transport");
        let rotation = transport.rotation_handle();

        // Write enough JSON-formatted entries to force several size rotations,
        // each producing a compressed `.gz` rotated file.
        drive(transport, |writer| async move {
            for i in 0..6 {
                writer
                    .write(json_log("info", &format!("gz-entry-{i}")))
                    .await
                    .expect("write");
            }
            writer.close().await.expect("close");
        });

        // Confirm there are .gz rotated files present before shipping.
        let gz_before: Vec<_> = rotation
            .list_rotated_files()
            .expect("list")
            .into_iter()
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("gz"))
            .collect();
        assert!(
            !gz_before.is_empty(),
            "expected at least one .gz rotated file, found none"
        );

        let captured = Arc::new(Mutex::new(Vec::<LogInfo>::new()));
        let target = CaptureIngest(Arc::clone(&captured));
        let stats = futures::executor::block_on(ship_rotated_files(
            &rotation,
            &target,
            10,
            thread_spawner,
        ))
        .expect("ship_rotated_files");

        assert_eq!(stats.ship_failures, 0, "no ship failures expected");
        assert!(
            stats.entries_shipped >= 1,
            "should have shipped entries from the gz files, shipped {}",
            stats.entries_shipped
        );

        // Every .gz file we saw before is now gone.
        for p in &gz_before {
            assert!(!p.exists(), "gz file should be deleted after shipping: {p:?}");
        }

        // Captured entries are real LogInfos (decompressed + parsed), not garbage.
        let entries = captured.lock().unwrap();
        assert!(!entries.is_empty());
        for e in entries.iter() {
            assert!(
                e.message.starts_with("gz-entry-"),
                "decompressed entry has unexpected message: {:?}",
                e.message
            );
        }
    }
}
