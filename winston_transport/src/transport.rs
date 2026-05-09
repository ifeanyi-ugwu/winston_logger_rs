use std::{future::Future, pin::Pin};

use logform::LogInfo;
use whatwg_streams::{
    ReadableSource, ReadableStreamDefaultController, StreamResult, WritableSink,
};

use crate::log_query::LogQuery;

/// The transport contract.
///
/// A transport is a `WritableSink<LogInfo>` that the pipeline drives through a
/// `WritableStream` — writes are serialized into the sink and backpressure is
/// applied by the stream's queuing strategy. Optionally a transport exposes a
/// streaming query through [`Transport::query_handle`].
///
/// There is no longer a sync/async split. Sinks whose work is synchronous just
/// do `async fn write(...) { sync_work(); Ok(()) }` and the future returns
/// ready immediately; sinks that perform real async I/O `await` it inside
/// `write`.
///
/// # Why query is a separate handle
///
/// `WritableSink::write` takes `&mut self` and the `WritableStream` owns the
/// sink for its lifetime, so once the Logger spawns a transport's stream, the
/// transport instance itself is no longer accessible. Query nevertheless has
/// to keep working — it scans past entries from the underlying store. The
/// solution is to *extract* a [`DynQueryHandle`] from the transport before
/// consuming it. The handle holds whatever stateless config the query needs
/// (a file path, a DB connection, an HTTP base URL) and opens a fresh
/// [`ReadableSource`] per call. Default `query_handle` returns `None`.
pub trait Transport: WritableSink<LogInfo> + Send + Sync + 'static {
    fn query_handle(&self) -> Option<Box<dyn DynQueryHandle>> {
        None
    }
}

/// A handle the Logger keeps after the transport itself is consumed by the
/// `WritableStream`. Each call to [`DynQueryHandle::query`] opens a fresh
/// streaming source over the underlying store.
pub trait DynQueryHandle: Send + Sync + 'static {
    fn query(&self, options: &LogQuery) -> Option<Box<dyn DynReadableSource>>;
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
