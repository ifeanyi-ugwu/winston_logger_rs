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

use logform::LogInfo;
use whatwg_streams::{CountQueuingStrategy, ReadableStream, StreamError, StreamResult};

use crate::{BoxedReadableSource, DynIngestHandle, DynReadableSource};

/// Drain `source` into `target` in batches of `batch_size`, returning the
/// number of entries shipped on success.
///
/// `spawn_fn` is the same kind of spawner you hand to a Logger: it must accept
/// a `Pin<Box<dyn Future<Output = ()> + Send + 'static>>` and arrange for it
/// to run to completion. The returned `WritableStream` task drives the
/// underlying source's `pull`.
///
/// On error, the entries already shipped stay shipped — there's no rollback.
/// Callers that need transactional semantics should batch into a
/// transport-specific store first, ingest from there, and treat any failure
/// as "retry the remainder."
///
/// # Example
///
/// ```ignore
/// # use winston_transport::{proxy::pipe_to_ingest, DynIngestHandle, DynReadableSource};
/// # async fn ex(
/// #     source: Box<dyn DynReadableSource>,
/// #     target: &dyn DynIngestHandle,
/// # ) -> Result<(), winston_transport::__Stub> {
/// let shipped = pipe_to_ingest(
///     source,
///     target,
///     /*batch_size*/ 100,
///     |fut| { std::thread::spawn(move || futures::executor::block_on(fut)); },
/// ).await?;
/// println!("shipped {shipped} entries");
/// # Ok(()) }
/// ```
pub async fn pipe_to_ingest<F, R>(
    source: Box<dyn DynReadableSource>,
    target: &dyn DynIngestHandle,
    batch_size: usize,
    spawn_fn: F,
) -> StreamResult<usize>
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
    Ok(total)
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

        let total = futures::executor::block_on(pipe_to_ingest(
            source,
            &target,
            3,
            thread_spawner,
        ))
        .expect("pipe_to_ingest");

        assert_eq!(total, 7);
        let got = captured.lock().unwrap();
        assert_eq!(got.len(), 7);
        assert_eq!(got[0].message, "entry 0");
        assert_eq!(got[6].message, "entry 6");
    }
}
