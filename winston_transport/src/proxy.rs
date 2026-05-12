//! Proxy primitives.
//!
//! Building blocks for moving log entries from one transport to another, on
//! top of the [`Transport`](crate::Transport) trait's `query_handle` and
//! `ingest_handle` methods.
//!
//! The atomic operation is [`pipe_to_ingest`] — drain a single
//! [`DynReadableSource`] into a single [`DynIngestHandle`] in batches. Higher-
//! level helpers (one-shot directory shipping, periodic timers, multi-target
//! fan-out) compose this primitive.

use std::{future::Future, pin::Pin};

use futures::future::join_all;
use logform::LogInfo;
use whatwg_streams::{CountQueuingStrategy, ReadableStream, StreamError, StreamResult};

use crate::{BoxedReadableSource, DynIngestHandle, DynReadableSource};

/// Proof that a drain completed — every entry the source produced was handed
/// to (and acknowledged by) the target.
///
/// Returned by [`pipe_to_ingest`] only on full success, and by
/// [`FanOutStats::into_drain_receipt`] only when every fan-out target
/// succeeded. It exists so that source-side commit operations that *delete*
/// the just-shipped data — most notably
/// [`MongoDBConsumeToken::delete_consumed`](../../winston_mongodb/struct.MongoDBConsumeToken.html#method.delete_consumed)
/// — can require one as a parameter, turning "I deleted the source before the
/// ship actually completed" from a silent data-loss footgun into a
/// type error.
///
/// Non-`Clone`, consumed when passed to a commit op — one drain, one receipt,
/// one commit. Pass the receipt from the drain that shipped *this* source's
/// data; the type system can't verify that linkage, so don't mix receipts
/// from different drains.
#[derive(Debug)]
pub struct DrainReceipt {
    entries_shipped: usize,
}

impl DrainReceipt {
    /// How many entries the drain shipped.
    pub fn entries_shipped(&self) -> usize {
        self.entries_shipped
    }
}

/// Drain `source` into `target` in batches of `batch_size`.
///
/// `spawn_fn` is the same kind of spawner you hand to a Logger: it must accept
/// a `Pin<Box<dyn Future<Output = ()> + Send + 'static>>` and arrange for it
/// to run to completion. The returned `WritableStream` task drives the
/// underlying source's `pull`.
///
/// Returns a [`DrainReceipt`] on full success — pass it to a source-side
/// commit op (e.g. `MongoDBConsumeToken::delete_consumed`) to authorize
/// deleting the just-shipped data.
///
/// On error, the entries already shipped stay shipped — there's no rollback,
/// and **no receipt is returned**, so the source-side delete can't happen.
/// The contract is at-least-once: on failure, leave the source intact and
/// let the next run re-ship; the target must tolerate duplicates (be
/// idempotent — dedup on a stable key) or you accept them.
///
/// # Example
///
/// ```ignore
/// # use winston_transport::{proxy::pipe_to_ingest, DynIngestHandle, DynReadableSource};
/// # async fn ex(
/// #     source: Box<dyn DynReadableSource>,
/// #     target: &dyn DynIngestHandle,
/// # ) {
/// let receipt = pipe_to_ingest(
///     source,
///     target,
///     /*batch_size*/ 100,
///     |fut| { std::thread::spawn(move || futures::executor::block_on(fut)); },
/// ).await.expect("ship failed");
/// println!("shipped {} entries", receipt.entries_shipped());
/// // ...now `token.delete_consumed(receipt)` is allowed.
/// # }
/// ```
pub async fn pipe_to_ingest<F, R>(
    source: Box<dyn DynReadableSource>,
    target: &dyn DynIngestHandle,
    batch_size: usize,
    spawn_fn: F,
) -> StreamResult<DrainReceipt>
where
    F: FnOnce(Pin<Box<dyn Future<Output = ()> + Send + 'static>>) -> R,
{
    let stream = ReadableStream::builder(BoxedReadableSource(source))
        .strategy(CountQueuingStrategy::new(64))
        .spawn(spawn_fn);

    let (_locked, reader) = stream
        .get_reader()
        .map_err(|_| StreamError::from("pipe_to_ingest: failed to acquire reader"))?;

    let cap = batch_size.max(1);
    let mut total = 0usize;
    let mut batch: Vec<LogInfo> = Vec::with_capacity(cap);

    while let Some(entry) = reader.read().await? {
        batch.push(entry);
        if batch.len() >= cap {
            let n = batch.len();
            target.ingest(std::mem::take(&mut batch)).await?;
            total += n;
        }
    }
    if !batch.is_empty() {
        let n = batch.len();
        target.ingest(batch).await?;
        total += n;
    }
    Ok(DrainReceipt {
        entries_shipped: total,
    })
}

/// Drain `source` into every target in `targets`, in parallel batches.
///
/// Each batch of up to `batch_size` entries is cloned across all targets and
/// shipped concurrently via `join_all`. The next batch is read only after
/// every target has finished the current one, so memory stays bounded at one
/// batch in flight regardless of fan-out width. The function returns when
/// the source is exhausted.
///
/// Independent per-target accounting via [`FanOutStats`]: a failing target
/// records the failure and the function keeps going — the remaining targets
/// still receive the rest of the source. This is the "best-effort
/// broadcast" semantic: every target gets as many entries as it can. To
/// require all-or-nothing semantics, inspect `stats` and only commit the
/// downstream side-effect (e.g. delete the source file) when every target
/// shows zero failures.
///
/// Empty target list short-circuits with `Ok(FanOutStats::default())` —
/// nothing to do.
///
/// # Example
///
/// ```ignore
/// let stats = fan_out_to_ingests(
///     source,
///     &[&*http_ingest, &*mongo_ingest, &*archive_file_ingest],
///     /*batch_size*/ 100,
///     |fut| { std::thread::spawn(move || futures::executor::block_on(fut)); },
/// ).await?;
/// // Only delete the source if every target took the full payload.
/// if stats.per_target_failed.iter().all(|f| *f == 0) {
///     std::fs::remove_file(path)?;
/// }
/// ```
pub async fn fan_out_to_ingests<F, R>(
    source: Box<dyn DynReadableSource>,
    targets: &[&dyn DynIngestHandle],
    batch_size: usize,
    spawn_fn: F,
) -> StreamResult<FanOutStats>
where
    F: FnOnce(Pin<Box<dyn Future<Output = ()> + Send + 'static>>) -> R,
{
    if targets.is_empty() {
        return Ok(FanOutStats::default());
    }

    let stream = ReadableStream::builder(BoxedReadableSource(source))
        .strategy(CountQueuingStrategy::new(64))
        .spawn(spawn_fn);

    let (_locked, reader) = stream
        .get_reader()
        .map_err(|_| StreamError::from("fan_out_to_ingests: failed to acquire reader"))?;

    let mut stats = FanOutStats {
        per_target_shipped: vec![0; targets.len()],
        per_target_failed: vec![0; targets.len()],
    };
    let cap = batch_size.max(1);
    let mut batch: Vec<LogInfo> = Vec::with_capacity(cap);

    while let Some(entry) = reader.read().await? {
        batch.push(entry);
        if batch.len() >= cap {
            send_batch_to_all(&mut batch, targets, &mut stats).await;
        }
    }
    if !batch.is_empty() {
        send_batch_to_all(&mut batch, targets, &mut stats).await;
    }
    Ok(stats)
}

/// Helper: send `batch` to every target concurrently, recording outcomes
/// into `stats`. Each target gets its own clone of the batch — slight
/// allocation cost in exchange for parallel writes.
async fn send_batch_to_all(
    batch: &mut Vec<LogInfo>,
    targets: &[&dyn DynIngestHandle],
    stats: &mut FanOutStats,
) {
    let chunk = std::mem::take(batch);
    let n = chunk.len();
    let mut futs = Vec::with_capacity(targets.len());
    for (i, target) in targets.iter().enumerate() {
        let chunk_for_target = chunk.clone();
        futs.push(async move { (i, target.ingest(chunk_for_target).await) });
    }
    let results = join_all(futs).await;
    for (i, result) in results {
        match result {
            Ok(()) => stats.per_target_shipped[i] += n,
            Err(_) => stats.per_target_failed[i] += 1,
        }
    }
}

/// Per-target accounting for [`fan_out_to_ingests`]. Indexed by the position
/// of the target in the input slice. A target that failed N batches has
/// `per_target_failed[i] == N`; the entries from those batches are NOT
/// counted in `per_target_shipped[i]`. Cross-checking: a target with zero
/// failures received `per_target_shipped[i]` entries in total.
#[derive(Debug, Default, Clone)]
pub struct FanOutStats {
    /// Number of entries successfully shipped to each target.
    pub per_target_shipped: Vec<usize>,
    /// Number of batches each target failed.
    pub per_target_failed: Vec<usize>,
}

impl FanOutStats {
    /// `true` iff there was at least one target and none of them failed any
    /// batch.
    pub fn all_targets_clean(&self) -> bool {
        !self.per_target_shipped.is_empty()
            && self.per_target_failed.iter().all(|&n| n == 0)
    }

    /// Consume the stats and produce a [`DrainReceipt`] *iff* every target
    /// took the full payload — i.e. there was at least one target and zero
    /// failures. `None` otherwise (no targets, or some target fell behind),
    /// which is the signal to NOT delete the source: the next run must
    /// re-ship so the lagging target catches up.
    pub fn into_drain_receipt(self) -> Option<DrainReceipt> {
        if self.all_targets_clean() {
            Some(DrainReceipt {
                // On a fully-clean fan-out every target got the same count.
                entries_shipped: self.per_target_shipped.first().copied().unwrap_or(0),
            })
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use whatwg_streams::{ReadableSource, ReadableStreamDefaultController, StreamResult};

    /// Vec-backed source: yields one chunk per pull until exhausted.
    struct VecSource(std::vec::IntoIter<LogInfo>);

    impl ReadableSource<LogInfo> for VecSource {
        async fn pull(
            &mut self,
            controller: &mut ReadableStreamDefaultController<LogInfo>,
        ) -> StreamResult<()> {
            match self.0.next() {
                Some(entry) => {
                    let _ = controller.enqueue(entry);
                }
                None => {
                    let _ = controller.close();
                }
            }
            Ok(())
        }
    }

    /// Vec-backed ingest sink: every batch is appended to a shared store
    /// the test reads back to assert what got shipped.
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

    fn thread_spawner(fut: Pin<Box<dyn Future<Output = ()> + Send + 'static>>) {
        std::thread::spawn(move || futures::executor::block_on(fut));
    }

    #[test]
    fn pipe_to_ingest_drains_and_batches() {
        let entries: Vec<LogInfo> = (0..7)
            .map(|i| LogInfo::new("info", &format!("entry {i}")))
            .collect();
        let source: Box<dyn DynReadableSource> =
            Box::new(VecSource(entries.into_iter()));

        let captured = Arc::new(Mutex::new(Vec::new()));
        let target = CaptureIngest(Arc::clone(&captured));

        let receipt = futures::executor::block_on(pipe_to_ingest(
            source,
            &target,
            3,
            thread_spawner,
        ))
        .expect("pipe_to_ingest");

        assert_eq!(receipt.entries_shipped(), 7);
        let got = captured.lock().unwrap();
        assert_eq!(got.len(), 7);
        assert_eq!(got[0].message, "entry 0");
        assert_eq!(got[6].message, "entry 6");
    }

    #[test]
    fn fan_out_to_ingests_broadcasts_to_all_targets() {
        let entries: Vec<LogInfo> = (0..5)
            .map(|i| LogInfo::new("info", &format!("e{i}")))
            .collect();
        let source: Box<dyn DynReadableSource> =
            Box::new(VecSource(entries.into_iter()));

        let c1 = Arc::new(Mutex::new(Vec::new()));
        let c2 = Arc::new(Mutex::new(Vec::new()));
        let c3 = Arc::new(Mutex::new(Vec::new()));
        let t1 = CaptureIngest(Arc::clone(&c1));
        let t2 = CaptureIngest(Arc::clone(&c2));
        let t3 = CaptureIngest(Arc::clone(&c3));

        let stats = futures::executor::block_on(fan_out_to_ingests(
            source,
            &[&t1, &t2, &t3],
            /*batch_size*/ 2,
            thread_spawner,
        ))
        .expect("fan_out_to_ingests");

        assert_eq!(stats.per_target_shipped, vec![5, 5, 5]);
        assert_eq!(stats.per_target_failed, vec![0, 0, 0]);
        assert!(stats.all_targets_clean());

        for store in [&c1, &c2, &c3] {
            let got = store.lock().unwrap();
            assert_eq!(got.len(), 5);
            assert_eq!(got[0].message, "e0");
            assert_eq!(got[4].message, "e4");
        }

        // A clean fan-out yields a receipt with the (shared) shipped count.
        let receipt = stats.into_drain_receipt().expect("clean fan-out → Some");
        assert_eq!(receipt.entries_shipped(), 5);
    }

    #[test]
    fn fan_out_to_ingests_empty_targets_short_circuits() {
        let source: Box<dyn DynReadableSource> =
            Box::new(VecSource(vec![LogInfo::new("info", "x")].into_iter()));

        let stats = futures::executor::block_on(fan_out_to_ingests(
            source,
            &[],
            /*batch_size*/ 1,
            thread_spawner,
        ))
        .expect("fan_out_to_ingests");

        assert!(stats.per_target_shipped.is_empty());
        assert!(stats.per_target_failed.is_empty());
        assert!(!stats.all_targets_clean(), "no targets → not clean");
        assert!(
            stats.into_drain_receipt().is_none(),
            "no targets → no receipt (don't delete the source)"
        );
    }

    /// A failing target records the failure but doesn't abort other targets.
    struct AlwaysFails;
    impl DynIngestHandle for AlwaysFails {
        fn ingest<'s>(
            &'s self,
            _logs: Vec<LogInfo>,
        ) -> Pin<Box<dyn Future<Output = StreamResult<()>> + Send + 's>> {
            Box::pin(async { Err(StreamError::from("simulated failure")) })
        }
    }

    #[test]
    fn fan_out_isolates_target_failures() {
        let entries: Vec<LogInfo> = (0..4)
            .map(|i| LogInfo::new("info", &format!("e{i}")))
            .collect();
        let source: Box<dyn DynReadableSource> =
            Box::new(VecSource(entries.into_iter()));

        let captured = Arc::new(Mutex::new(Vec::new()));
        let good = CaptureIngest(Arc::clone(&captured));
        let bad = AlwaysFails;

        let stats = futures::executor::block_on(fan_out_to_ingests(
            source,
            &[&good, &bad],
            /*batch_size*/ 2,
            thread_spawner,
        ))
        .expect("fan_out_to_ingests");

        // Good target gets everything; bad target records two batch failures.
        assert_eq!(stats.per_target_shipped, vec![4, 0]);
        assert_eq!(stats.per_target_failed, vec![0, 2]);
        assert_eq!(captured.lock().unwrap().len(), 4);
        assert!(!stats.all_targets_clean());
        assert!(
            stats.into_drain_receipt().is_none(),
            "a failing target → no receipt (don't delete the source)"
        );
    }
}
