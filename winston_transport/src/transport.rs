use std::{future::Future, pin::Pin};

use logform::{FormattedEntry, LogInfo};
use whatwg_streams::{
    ReadableSource, ReadableStreamDefaultController, StreamResult, WritableSink,
    WritableStreamDefaultController,
};

use crate::log_query::LogQuery;

/// The transport contract — what a log destination implements.
///
/// A transport receives each already-formatted entry through [`log`](Transport::log)
/// and writes it to its backing store. The Logger applies level filtering and
/// formatting upstream, so an entry reaching `log` has passed this transport's
/// level gate and carries both the rendered line (`entry.to_string()`) and the
/// structured record (`entry.info`) — the impl chooses which to persist. A
/// string sink writes `entry.to_string()`; a structured sink reads `entry.info`.
///
/// A transport never touches the streaming machinery. The Logger wraps it in a
/// [`TransportSink`] and drives that through a `WritableStream`, which
/// serializes `log` calls and applies backpressure via its queuing strategy.
///
/// There is no sync/async split: a synchronous sink writes
/// `async fn log(...) { sync_work(); Ok(()) }` and its future resolves
/// immediately; a sink doing real I/O `await`s it inside `log`.
///
/// Optionally a transport exposes a streaming query over past entries through
/// [`query_handle`](Transport::query_handle) and an out-of-band ingestion
/// endpoint through [`ingest_handle`](Transport::ingest_handle).
///
/// # Why query and ingest are separate handles
///
/// `log` takes `&mut self` and the `WritableStream` owns the transport for its
/// lifetime, so once the Logger spawns a transport's stream, the transport
/// instance itself is no longer accessible. Query and out-of-band ingest
/// nevertheless have to keep working — query scans past entries from the
/// underlying store; ingest accepts a batch produced elsewhere (e.g., by a
/// periodic proxy from another transport).
///
/// The solution is to *extract* handles from the transport before consuming
/// it. The handles hold whatever stateless config their work needs (a file
/// path, a DB connection string, an HTTP client + URL) and open whatever
/// per-call resources they need (a fresh cursor, a fresh append-mode file
/// handle, a single batched POST). Default both return `None`.
pub trait Transport: Send + Sync + 'static {
    /// Set up the sink before the first `log` — open a connection, create
    /// indexes, etc. Runs once. Default does nothing.
    fn start(&mut self) -> impl Future<Output = StreamResult<()>> + Send {
        async { Ok(()) }
    }

    /// Write one formatted entry to the backing store.
    ///
    /// Leaving the returned future unresolved applies backpressure — the stream
    /// won't pull the next entry until it resolves. Returning `Err` surfaces to
    /// the stream as a write error.
    fn log(
        &mut self,
        entry: FormattedEntry,
    ) -> impl Future<Output = StreamResult<()>> + Send;

    /// Flush and finalize when the Logger tears the transport down.
    ///
    /// Takes `self` by value so a buffering sink can drain its accumulated
    /// state. Called on graceful shutdown only — teardown that must always run
    /// belongs in a `Drop` impl. Default flushes nothing.
    fn close(self) -> impl Future<Output = StreamResult<()>> + Send
    where
        Self: Sized,
    {
        async { Ok(()) }
    }

    fn query_handle(&self) -> Option<Box<dyn DynQueryHandle>> {
        None
    }

    /// Long-lived handle the Logger keeps after the transport's writer half
    /// is consumed by the `WritableStream`. Each `ingest(batch)` call accepts
    /// a `Vec<LogInfo>` and writes it to the transport's underlying store,
    /// independent of the live `log` path the Logger is feeding.
    ///
    /// Used to implement transport-to-transport proxying: drain a source
    /// transport's accumulated logs (via `query_handle`) and feed them into
    /// a target's `ingest_handle`. Default returns `None` for transports
    /// that aren't sensible ingestion targets (stdout/stderr, etc.).
    fn ingest_handle(&self) -> Option<Box<dyn DynIngestHandle>> {
        None
    }
}

/// Adapts any [`Transport`] into the `WritableSink<FormattedEntry>` the pipeline
/// drives. The Logger wraps each transport in this before building its
/// `WritableStream`; transport authors never construct or see it. This is the
/// single place the stream-sink protocol is implemented, so a transport only
/// ever supplies the domain methods (`start`/`log`/`close`).
pub struct TransportSink<T>(pub T);

impl<T: Transport> WritableSink<FormattedEntry> for TransportSink<T> {
    async fn start(
        &mut self,
        _controller: &mut WritableStreamDefaultController,
    ) -> StreamResult<()> {
        self.0.start().await
    }

    async fn write(
        &mut self,
        entry: FormattedEntry,
        _controller: &mut WritableStreamDefaultController,
    ) -> StreamResult<()> {
        self.0.log(entry).await
    }

    async fn close(self) -> StreamResult<()> {
        self.0.close().await
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
/// to the underlying store independently of the live `log` path.
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
