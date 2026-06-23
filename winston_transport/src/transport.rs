use std::{future::Future, pin::Pin};

use logform::{FormattedEntry, LogInfo};
use whatwg_streams::{
    ReadableSource, ReadableStreamDefaultController, StreamResult, WritableSink,
};

use crate::log_query::LogQuery;

/// The transport contract.
///
/// A transport is a `WritableSink<FormattedEntry>` that the pipeline drives
/// through a `WritableStream` — writes are serialized into the sink and
/// backpressure is applied by the stream's queuing strategy. A string sink
/// writes `entry.to_string()` (the rendered line); a structured sink reads
/// `entry.info`. Optionally a transport exposes a
/// streaming query through [`Transport::query_handle`] and an out-of-band
/// ingestion endpoint through [`Transport::ingest_handle`].
///
/// There is no longer a sync/async split. Sinks whose work is synchronous just
/// do `async fn write(...) { sync_work(); Ok(()) }` and the future returns
/// ready immediately; sinks that perform real async I/O `await` it inside
/// `write`.
///
/// # Why query and ingest are separate handles
///
/// `WritableSink::write` takes `&mut self` and the `WritableStream` owns the
/// sink for its lifetime, so once the Logger spawns a transport's stream, the
/// transport instance itself is no longer accessible. Query and out-of-band
/// ingest nevertheless have to keep working — query scans past entries from
/// the underlying store; ingest accepts a batch produced elsewhere (e.g., by
/// a periodic proxy from another transport).
///
/// The solution is to *extract* handles from the transport before consuming
/// it. The handles hold whatever stateless config their work needs (a file
/// path, a DB connection string, an HTTP client + URL) and open whatever
/// per-call resources they need (a fresh cursor, a fresh append-mode file
/// handle, a single batched POST). Default both return `None`.
pub trait Transport: WritableSink<FormattedEntry> + Send + Sync + 'static {
    fn query_handle(&self) -> Option<Box<dyn DynQueryHandle>> {
        None
    }

    /// Long-lived handle the Logger keeps after the transport's writer half
    /// is consumed by the `WritableStream`. Each `ingest(batch)` call accepts
    /// a `Vec<LogInfo>` and writes it to the transport's underlying store,
    /// independent of the live `WritableSink` path the Logger is feeding.
    ///
    /// Used to implement transport-to-transport proxying: drain a source
    /// transport's accumulated logs (via `query_handle`) and feed them into
    /// a target's `ingest_handle`. Default returns `None` for transports
    /// that aren't sensible ingestion targets (stdout/stderr, etc.).
    fn ingest_handle(&self) -> Option<Box<dyn DynIngestHandle>> {
        None
    }
}

/// A handle the Logger keeps after the transport itself is consumed by the
/// `WritableStream`. Each call to [`DynQueryHandle::query`] opens a fresh
/// streaming source over the underlying store.
pub trait DynQueryHandle: Send + Sync + 'static {
    fn query(&self, options: &LogQuery) -> Option<Box<dyn DynReadableSource>>;

    /// Synchronous collect for in-memory transports. Returns all matching
    /// entries directly, bypassing `ReadableStream` task/channel overhead.
    /// Network and file transports leave this as `None`; the caller falls
    /// back to the stream path via [`DynQueryHandle::query`].
    fn query_sync(&self, _options: &LogQuery) -> Option<Vec<LogInfo>> {
        None
    }
}

/// Handle for out-of-band batch ingestion into a transport. Holds clonable
/// config (URL+client, file path, DB connection string) and writes each batch
/// to the underlying store independently of the live `WritableSink` path.
///
/// Implementations should be safe to call concurrently with the transport's
/// live `write` — typically by opening their own per-call resources rather
/// than sharing mutable state with the live writer.
pub trait DynIngestHandle: Send + Sync + 'static {
    fn ingest<'s>(
        &'s self,
        logs: Vec<LogInfo>,
    ) -> Pin<Box<dyn Future<Output = StreamResult<()>> + Send + 's>>;
}

/// Object-safe wrapper for [`ReadableSource<LogInfo>`].
///
/// The base `ReadableSource` trait uses `impl Future` returns and isn't
/// dyn-compatible. `DynReadableSource` exposes the same shape with `BoxFuture`
/// returns so query handles can return `Box<dyn DynReadableSource>`. A blanket
/// impl converts any `S: ReadableSource<LogInfo> + Send + 'static`
/// automatically — implementers don't write this themselves.
pub trait DynReadableSource: Send + 'static {
    fn pull_dyn<'s>(
        &'s mut self,
        controller: &'s mut ReadableStreamDefaultController<LogInfo>,
    ) -> Pin<Box<dyn Future<Output = StreamResult<()>> + Send + 's>>;
}

impl<S> DynReadableSource for S
where
    S: ReadableSource<LogInfo> + Send + 'static,
{
    fn pull_dyn<'s>(
        &'s mut self,
        controller: &'s mut ReadableStreamDefaultController<LogInfo>,
    ) -> Pin<Box<dyn Future<Output = StreamResult<()>> + Send + 's>> {
        Box::pin(<S as ReadableSource<LogInfo>>::pull(self, controller))
    }
}

/// Wraps a `Box<dyn DynReadableSource>` so it can be fed to
/// `ReadableStream::builder(...)` — the `Box` itself isn't a `ReadableSource`,
/// but this thin newtype is.
pub struct BoxedReadableSource(pub Box<dyn DynReadableSource>);

impl ReadableSource<LogInfo> for BoxedReadableSource {
    async fn pull(
        &mut self,
        controller: &mut ReadableStreamDefaultController<LogInfo>,
    ) -> StreamResult<()> {
        self.0.pull_dyn(controller).await
    }
}
