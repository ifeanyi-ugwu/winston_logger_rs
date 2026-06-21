//! Proxying: moving log entries from one transport to another.
//!
//! Built on the [`Transport`](crate::Transport) trait's three handle kinds —
//! `query_handle` (non-destructive read), `ingest_handle` (write), and the
//! transport-specific *destructive* reads
//! ([`FileRotateHandle::rotate_and_drain`](../../winston_file/struct.FileRotateHandle.html#method.rotate_and_drain),
//! [`MongoDBQueryHandle::query_consuming`](../../winston_mongodb/struct.MongoDBQueryHandle.html#method.query_consuming)).
//!
//! # The delivery model
//!
//! Moving data across a process boundary that might crash, you get to pick:
//!
//! - **at-most-once** — delete from the source *before* the target confirms.
//!   Crash in between ⇒ lost. Never the default here.
//! - **at-least-once** — target confirms, *then* delete from the source.
//!   Crash in between ⇒ the next run re-ships ⇒ duplicate at the target.
//!   **This is what these primitives give you.**
//! - **exactly-once** = at-least-once **+ an idempotent target** (one that
//!   dedups on a stable key). The target absorbs the re-ship, so it converges
//!   with no net duplicates.
//!
//! There is no fourth option without distributed-transaction machinery the
//! stream layer can't provide. So: the building blocks are at-least-once
//! (they never lose data); "exactly-once" means layering idempotency on top.
//!
//! # The pieces
//!
//! **Sources** (where unshipped data lives across a crash):
//!
//! | source | destructive read | crash-recoverable? |
//! |---|---|---|
//! | `DailyRotateFile` | rotated files via `rotation_handle().list_rotated_files()` | ✅ rotated files have predictable names; re-discovered every pass |
//! | `MongoDBTransport` | `query_handle().query_consuming(opts)` → `(source, token)` | ✅ docs stay in the collection until `token.delete_consumed(receipt)` |
//! | `FileTransport` | `rotate_handle().rotate_and_drain(spawn)` → `FileDrain` | ⚠️ a crash after the rename orphans the `*.drain-*` file — recover with `rotate_handle().list_pending_drains()` at startup |
//! | `HttpTransport`, stdout/stderr | — | n/a (no local store to read from) |
//!
//! **Targets** (the write side):
//!
//! | target | `ingest_handle` | idempotent variant |
//! |---|---|---|
//! | `MongoDBTransport` | plain `insert_many`, auto `_id` | `idempotent_ingest_handle()` — keys on [`content_id`] as `_id`; re-ship = no-op |
//! | `HttpTransport` | POST; each entry carries `_id = content_id` | the *endpoint* must dedup on `_id` (out of our hands) |
//! | `FileTransport`, `DailyRotateFile` | append a line | **not idempotent** — appending the same line twice gives two lines, and a flat file has no key-based upsert |
//! | stdout/stderr | — | n/a |
//!
//! **Glue:**
//!
//! - [`pipe_to_ingest`] — drain one source into one target, batched. Returns
//!   a [`DrainReceipt`] on full success; that receipt is what authorizes the
//!   source-side delete (`MongoDBConsumeToken::delete_consumed` *requires* it,
//!   so "delete before the ship completed" is a type error).
//! - [`fan_out_to_ingests`] — broadcast one source to N targets in parallel.
//!   `FanOutStats::into_drain_receipt()` yields `Some` only if every target
//!   took the whole payload (don't delete the source otherwise).
//! - [`ship_rotated_files`](../../winston_daily_rotate_file/archive/fn.ship_rotated_files.html)
//!   — the DailyRotate workflow: list rotated files (decompressing `.gz`),
//!   drain each into a target, delete on success. Owns the delete internally
//!   so it's safe by construction.
//! - [`content_id`] — the dedup key: a recursively-key-sorted sha256 of the
//!   entry, stable across re-reads.
//!
//! # The rules
//!
//! 1. **Delete the source only after a [`DrainReceipt`].** Enforced by the
//!    type system for `delete_consumed`; for the `ship_rotated_files` /
//!    `remove_file` path it's enforced because the delete is on the `Ok`
//!    branch of the same function.
//! 2. **One proxy per source.** Two processes draining the same collection or
//!    file both ship overlapping sets ⇒ duplicates (absorbed only by an
//!    idempotent target).
//! 3. **No overlapping ship passes.** If pass N+1 starts before pass N
//!    deleted the file it's still draining, both ship it. Serialize passes
//!    (a process-local mutex, or a single scheduler).
//! 4. **Flat-file targets are not idempotent.** Re-ship after a crash
//!    re-appends. For exactly-once, the target must be a keyed store —
//!    `idempotent_ingest_handle` MongoDB, or a dedup-aware HTTP endpoint.
//! 5. **Recover orphaned drains.** If you use `FileTransport::rotate_and_drain`,
//!    call `list_pending_drains()` at startup and re-ship anything left over.
//!
//! # The exactly-once-ish recipe
//!
//! `DailyRotateFile` (source) → `MongoDBTransport::idempotent_ingest_handle`
//! (target), keyed on `content_id`:
//!
//! 1. DailyRotate writes `logform::json()` lines (canonical — sorted keys),
//!    rotating by date or size.
//! 2. Periodically: for each rotated file, drain it via [`pipe_to_ingest`]
//!    (it's `ship_rotated_files`' loop) into the idempotent Mongo handle;
//!    delete the file once the `DrainReceipt` is in hand.
//! 3. Crash anywhere ⇒ the rotated file survives ⇒ the next pass re-ships ⇒
//!    upserts no-op the docs that made it, insert the rest ⇒ converges. No
//!    loss, no net duplicates — given rules 2 and 3 above.

use std::{future::Future, pin::Pin};

use futures::future::join_all;
use logform::LogInfo;
use serde_json::Value;
use sha2::{Digest, Sha256};
use whatwg_streams::{CountQueuingStrategy, ReadableStream, StreamError, StreamResult};

use crate::{BoxedReadableSource, DynIngestHandle, DynReadableSource};

/// A stable content-derived identifier for a log entry — the dedup key that
/// turns at-least-once delivery into effectively exactly-once *for an
/// idempotent target* (one that upserts on this key).
///
/// It's `sha256` of a *canonical* JSON serialization of the entry: an object
/// `{"level": ..., "message": ..., ...meta}` with **all keys sorted
/// recursively**. The recursive sort means the id doesn't depend on
/// `HashMap` iteration order, `serde_json`'s `Map` backing, or which order a
/// formatter happened to write the meta fields — re-reading a persisted entry
/// (e.g. a JSON line from a rotated file) and hashing it again yields the same
/// id, which is exactly what a re-ship after a crash needs.
///
/// Returned as a 64-char lowercase hex string, suitable as a MongoDB `_id` or
/// an HTTP idempotency key.
///
/// Caveat: two log calls that produce byte-identical `(level, message, meta)`
/// get the *same* id and will be deduped to one. For logs that's almost
/// always desirable; if you have genuinely-distinct events that must not
/// collide, add a disambiguator to `meta` (a sequence number, a UUID) before
/// they're persisted.
pub fn content_id(info: &LogInfo) -> String {
    let mut obj = serde_json::Map::new();
    obj.insert("level".to_string(), Value::String(info.level.clone()));
    obj.insert("message".to_string(), Value::String(info.message.clone()));
    for (k, v) in &info.meta {
        obj.insert(k.to_string(), v.clone());
    }
    let canonical = canonicalize(&Value::Object(obj));
    // `serde_json::to_string` on a value we built in sorted order preserves
    // that order regardless of the `Map` backing.
    let bytes = serde_json::to_vec(&canonical).unwrap_or_default();
    let digest = Sha256::digest(&bytes);
    hex::encode(digest)
}

/// Recursively rebuild a JSON value with every object's keys sorted.
fn canonicalize(v: &Value) -> Value {
    match v {
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            let mut out = serde_json::Map::new();
            for (k, val) in entries {
                out.insert(k.clone(), canonicalize(val));
            }
            Value::Object(out)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(canonicalize).collect()),
        other => other.clone(),
    }
}

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
    fn content_id_is_stable_and_order_independent() {
        // Build the "same" entry two different ways: meta keys inserted in
        // different orders, plus a nested object also in different orders.
        let a = LogInfo::new("info", "hello")
            .with_meta("b", serde_json::json!({"y": 2, "x": 1}))
            .with_meta("a", 1);
        let b = LogInfo::new("info", "hello")
            .with_meta("a", 1)
            .with_meta("b", serde_json::json!({"x": 1, "y": 2}));
        assert_eq!(content_id(&a), content_id(&b), "key order must not matter");
        assert_eq!(content_id(&a).len(), 64, "sha256 hex is 64 chars");

        // A different message → different id.
        let c = LogInfo::new("info", "goodbye");
        assert_ne!(content_id(&a), content_id(&c));
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

    //
    // These encode the at-least-once / exactly-once semantics as executable
    // specs: a pipe that fails mid-stream returns no `DrainReceipt`, and a
    // retry of the whole source re-ships the batches that already made it.
    // Whether that re-ship produces *net* duplicates depends entirely on the
    // target: a plain (append-style) target shows them; an idempotent target
    // (dedups on `content_id`) absorbs them.

    use std::collections::HashSet;

    /// Target that fails its Nth `ingest` call exactly once, then behaves
    /// forever after. When `idempotent`, it dedups stored entries on
    /// `content_id`; otherwise it appends every entry (so duplicates are
    /// visible in `plain`).
    #[derive(Clone)]
    struct FailingTarget {
        state: Arc<Mutex<FtState>>,
        idempotent: bool,
    }
    struct FtState {
        calls: usize,
        fail_on_call: usize,
        plain: Vec<LogInfo>,
        deduped: HashSet<String>,
    }
    impl FailingTarget {
        fn new(fail_on_call: usize, idempotent: bool) -> Self {
            Self {
                state: Arc::new(Mutex::new(FtState {
                    calls: 0,
                    fail_on_call,
                    plain: Vec::new(),
                    deduped: HashSet::new(),
                })),
                idempotent,
            }
        }
        fn plain_len(&self) -> usize {
            self.state.lock().unwrap().plain.len()
        }
        fn deduped_len(&self) -> usize {
            self.state.lock().unwrap().deduped.len()
        }
    }
    impl DynIngestHandle for FailingTarget {
        fn ingest<'s>(
            &'s self,
            logs: Vec<LogInfo>,
        ) -> Pin<Box<dyn Future<Output = StreamResult<()>> + Send + 's>> {
            let idempotent = self.idempotent;
            let state = Arc::clone(&self.state);
            Box::pin(async move {
                let mut st = state.lock().unwrap();
                st.calls += 1;
                if st.calls == st.fail_on_call {
                    return Err(StreamError::from("simulated mid-stream failure"));
                }
                if idempotent {
                    for e in &logs {
                        st.deduped.insert(content_id(e));
                    }
                } else {
                    st.plain.extend(logs);
                }
                Ok(())
            })
        }
    }

    fn four_entries() -> Box<dyn DynReadableSource> {
        Box::new(VecSource(
            (0..4)
                .map(|i| LogInfo::new("info", &format!("e{i}")))
                .collect::<Vec<_>>()
                .into_iter(),
        ))
    }

    /// At-least-once with a plain (non-idempotent) target: the run-1 failure
    /// (on the 2nd batch) yields no receipt, run 2 re-ships the whole source,
    /// and the first batch ends up at the target twice.
    #[test]
    fn at_least_once_plain_target_shows_duplicates_on_retry() {
        let target = FailingTarget::new(/*fail_on_call*/ 2, /*idempotent*/ false);

        // Run 1: batch [e0,e1] ships (call 1), batch [e2,e3] fails (call 2).
        let r1 = futures::executor::block_on(pipe_to_ingest(
            four_entries(),
            &target,
            /*batch_size*/ 2,
            thread_spawner,
        ));
        assert!(r1.is_err(), "the failing batch must surface as Err");
        assert_eq!(target.plain_len(), 2, "only the first batch made it");

        // Run 2: re-ship the whole source. Batch [e0,e1] ships AGAIN, then
        // [e2,e3]. Now e0,e1 are at the target twice.
        let r2 = futures::executor::block_on(pipe_to_ingest(
            four_entries(),
            &target,
            2,
            thread_spawner,
        ))
        .expect("retry should succeed (the target only fails once)");
        assert_eq!(r2.entries_shipped(), 4);
        assert_eq!(
            target.plain_len(),
            6,
            "non-idempotent target: 2 (run1) + 4 (run2) = 6, with e0/e1 duped"
        );
    }

    /// Same scenario, but the target dedups on `content_id`: the re-shipped
    /// first batch is a no-op, so the target converges to exactly the 4
    /// distinct entries — effectively exactly-once.
    #[test]
    fn at_least_once_idempotent_target_converges_on_retry() {
        let target = FailingTarget::new(/*fail_on_call*/ 2, /*idempotent*/ true);

        let r1 = futures::executor::block_on(pipe_to_ingest(
            four_entries(),
            &target,
            2,
            thread_spawner,
        ));
        assert!(r1.is_err());
        assert_eq!(target.deduped_len(), 2);

        let r2 = futures::executor::block_on(pipe_to_ingest(
            four_entries(),
            &target,
            2,
            thread_spawner,
        ))
        .expect("retry should succeed");
        assert_eq!(r2.entries_shipped(), 4);
        assert_eq!(
            target.deduped_len(),
            4,
            "idempotent target: re-ship absorbed, converges to 4 distinct"
        );
    }
}
