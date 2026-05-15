use std::{
    collections::VecDeque,
    future::Future,
    pin::Pin,
    sync::{Arc, Condvar, Mutex},
};

use futures::channel::mpsc as fmpsc;
use futures::StreamExt;
use logform::{Format, LogInfo};
use whatwg_streams::{
    CountQueuingStrategy, StreamResult, WritableSink, WritableStream,
    WritableStreamDefaultWriter,
};
use winston_transport::Transport;

use crate::{
    logger::TransportHandle,
    logger_levels::LoggerLevels,
    logger_options::LoggerOptions,
    logger_transport::LoggerTransport,
};

/// A runtime-agnostic task spawner.  Pass `tokio::runtime::Handle::spawn`,
/// `smol::spawn`, or any other executor's spawn primitive wrapped in an `Arc`.
pub type SpawnFn = Arc<dyn Fn(Pin<Box<dyn Future<Output = ()> + Send + 'static>>) + Send + Sync>;

/// The built-in spawner: runs each future on its own OS thread.
/// Used automatically when no spawner is provided.
pub fn default_spawner() -> SpawnFn {
    Arc::new(|fut: Pin<Box<dyn Future<Output = ()> + Send + 'static>>| {
        std::thread::spawn(move || {
            futures::executor::block_on(fut);
        });
    })
}

/// Single-threaded spawner: every spawned future runs on one shared OS thread,
/// multiplexed cooperatively via a `FuturesUnordered` set.
///
/// Use this when all transports are fully async / non-blocking. A transport
/// that performs synchronous blocking I/O inside `poll` will stall every other
/// task on the executor — pick `default_spawner` instead in that case.
pub fn single_threaded_spawner() -> SpawnFn {
    type BoxFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;
    let (tx, rx) = fmpsc::unbounded::<BoxFuture>();
    std::thread::Builder::new()
        .name("winston-executor".into())
        .spawn(move || {
            futures::executor::block_on(async move {
                let mut rx = rx;
                let mut tasks = futures::stream::FuturesUnordered::<BoxFuture>::new();
                loop {
                    futures::select! {
                        incoming = rx.next() => match incoming {
                            Some(fut) => tasks.push(fut),
                            None => break,
                        },
                        _ = tasks.select_next_some() => {},
                    }
                }
                while tasks.next().await.is_some() {}
            });
        })
        .expect("failed to spawn winston executor thread");
    Arc::new(move |fut| {
        let _ = tx.unbounded_send(fut);
    })
}


pub enum PipelineMessage {
    Entry(Arc<LogInfo>),
    Flush(Arc<(Mutex<bool>, Condvar)>),
    /// Add a transport at runtime; the fanout task spawns its WritableStream.
    AddTransport {
        handle: TransportHandle,
        transport: LoggerTransport,
    },
    /// Remove a transport by handle.
    RemoveTransport(TransportHandle),
    /// Replace global format/level/levels without touching transports.
    Reconfigure {
        format: Option<Arc<dyn Format<Input = LogInfo> + Send + Sync>>,
        level: Option<String>,
        levels: Option<LoggerLevels>,
    },
    /// Clear all transports, then optionally install a new set.
    Configure {
        format: Option<Arc<dyn Format<Input = LogInfo> + Send + Sync>>,
        level: Option<String>,
        levels: Option<LoggerLevels>,
        transports: Vec<(TransportHandle, LoggerTransport)>,
    },
    Shutdown,
}

// SAFETY: every variant's payload is Send + Sync.
unsafe impl Send for PipelineMessage {}
unsafe impl Sync for PipelineMessage {}


/// Sink-type-erased view of a `WritableStreamDefaultWriter<LogInfo, _>`.
///
/// Each transport has a different concrete `Sink` type, so the fanout task
/// can't store their writers in a homogeneous `Vec` directly. This trait
/// forwards the only two operations the fanout actually performs against a
/// writer — `write` and `close` — through dynamic dispatch.
pub(crate) trait ErasedWriter: Send + Sync {
    fn write<'a>(
        &'a self,
        info: LogInfo,
    ) -> Pin<Box<dyn Future<Output = StreamResult<()>> + Send + 'a>>;

    fn close<'a>(&'a self) -> Pin<Box<dyn Future<Output = StreamResult<()>> + Send + 'a>>;
}

impl<Sink> ErasedWriter for WritableStreamDefaultWriter<LogInfo, Sink>
where
    Sink: WritableSink<LogInfo> + Send + Sync + 'static,
{
    fn write<'a>(
        &'a self,
        info: LogInfo,
    ) -> Pin<Box<dyn Future<Output = StreamResult<()>> + Send + 'a>> {
        Box::pin(WritableStreamDefaultWriter::write(self, info))
    }

    fn close<'a>(&'a self) -> Pin<Box<dyn Future<Output = StreamResult<()>> + Send + 'a>> {
        Box::pin(WritableStreamDefaultWriter::close(self))
    }
}

/// One-shot factory the fanout invokes when admitting a transport.
///
/// Captures the typed transport, spawns its `WritableStream` pump task through
/// the supplied `SpawnFn`, and returns the sink-erased writer.
pub type TransportWriterBuilder =
    Box<dyn FnOnce(SpawnFn) -> Box<dyn ErasedWriter> + Send>;

/// Per-transport queue depth applied via `CountQueuingStrategy`.
///
/// A slow transport applies backpressure to the fanout task only once its
/// WritableStream queue fills; until then, `writer.write(...)` resolves
/// immediately and the fanout moves on. Sizing this is the per-transport
/// memory bound under sustained slowness.
pub const TRANSPORT_QUEUE_HIGH_WATER_MARK: usize = 1024;

/// Construct the type-erased builder used by `LoggerTransport`.
///
/// Wraps the typed transport in a `WritableStream` (whose pump task runs on
/// the spawner supplied at admission time), takes its writer, and erases the
/// Sink type parameter via `ErasedWriter`.
pub(crate) fn make_writer_builder<T>(transport: T) -> TransportWriterBuilder
where
    T: Transport,
{
    Box::new(move |spawn_fn: SpawnFn| {
        let stream = WritableStream::builder(transport)
            .strategy(CountQueuingStrategy::new(TRANSPORT_QUEUE_HIGH_WATER_MARK))
            .spawn(move |fut| spawn_fn(fut));

        // A freshly-built stream is never locked, so this can't fail. We drop
        // the locked-handle returned alongside the writer — the writer alone
        // keeps the stream task alive (it holds a clone of the command sender).
        let (_locked, writer) = stream
            .get_writer()
            .expect("fresh WritableStream cannot already be locked");
        Box::new(writer) as Box<dyn ErasedWriter>
    })
}


struct TransportSlot {
    handle: TransportHandle,
    level: Option<String>,
    transport_format: Option<Arc<dyn Format<Input = LogInfo> + Send + Sync>>,
    /// Dropping the writer alone does *not* trigger `WritableSink::close`;
    /// the fanout task awaits `writer.close()` explicitly before discarding
    /// the slot so transports get their drain-and-close lifecycle.
    writer: Box<dyn ErasedWriter>,
}


struct FanoutState {
    spawn_fn: SpawnFn,
    slots: Vec<TransportSlot>,
    global_format: Option<Arc<dyn Format<Input = LogInfo> + Send + Sync>>,
    global_level: Option<String>,
    levels: Option<LoggerLevels>,
    /// Entries logged before any transport was admitted. Drained into the
    /// slots the moment the first transport arrives.
    buffer: Arc<Mutex<VecDeque<Arc<LogInfo>>>>,
}

impl FanoutState {
    fn passes_level(&self, entry_level: &str, transport_level: Option<&String>) -> bool {
        let levels = match &self.levels {
            Some(l) => l,
            None => return true,
        };
        let effective = transport_level.or(self.global_level.as_ref());
        let effective = match effective {
            Some(l) => l,
            None => return true,
        };
        match (
            levels.get_severity(entry_level),
            levels.get_severity(effective),
        ) {
            (Some(entry_sev), Some(req_sev)) => entry_sev <= req_sev,
            _ => false,
        }
    }

    fn admit(&mut self, handle: TransportHandle, transport: LoggerTransport) {
        let level = transport.get_level().cloned();
        let transport_format = transport.get_format();
        let Some(builder) = transport.take_builder() else {
            return; // Already consumed (e.g. duplicate add) — ignore silently.
        };
        let writer = builder(Arc::clone(&self.spawn_fn));
        self.slots.push(TransportSlot {
            handle,
            level,
            transport_format,
            writer,
        });
    }

    /// Format an entry for a specific slot, applying transport-level format
    /// when present and falling back to the global format otherwise.
    fn format_for(&self, slot: &TransportSlot, entry: &LogInfo) -> Option<LogInfo> {
        match (&slot.transport_format, &self.global_format) {
            (Some(tf), _) => tf.transform(entry.clone()),
            (None, Some(gf)) => gf.transform(entry.clone()),
            (None, None) => Some(entry.clone()),
        }
    }

    /// Concurrently writes the entry to every transport whose level admits it.
    ///
    /// `join_all` keeps transports isolated within a single entry: a slow
    /// transport doesn't gate faster ones receiving the same chunk. Across
    /// entries the fanout is serial — the next entry waits until every
    /// transport accepted the current one, which is what bounds memory growth
    /// once each transport's `CountQueuingStrategy` queue fills.
    async fn fan_entry(&self, entry: &Arc<LogInfo>) {
        let writes = self.slots.iter().filter_map(|slot| {
            if !self.passes_level(&entry.level, slot.level.as_ref()) {
                return None;
            }
            let info = self.format_for(slot, entry)?;
            Some(async move {
                let _ = slot.writer.write(info).await;
            })
        });
        let _ = futures::future::join_all(writes).await;
    }

    async fn drain_buffer(&mut self) {
        let buffered: Vec<Arc<LogInfo>> = {
            let mut buf = self.buffer.lock().unwrap();
            buf.drain(..).collect()
        };
        for entry in &buffered {
            self.fan_entry(entry).await;
        }
    }

    async fn process_entry(&mut self, entry: Arc<LogInfo>) {
        if entry.message.is_empty() && entry.meta.is_empty() {
            return;
        }

        if self.slots.is_empty() {
            self.buffer.lock().unwrap().push_back(Arc::clone(&entry));
            eprintln!(
                "[winston] Attempt to write logs with no transports, which can increase memory usage: {}",
                entry.message
            );
            return;
        }

        self.drain_buffer().await;
        self.fan_entry(&entry).await;
    }

    /// Close every transport's writer in parallel and drop the slots.
    ///
    /// Awaiting `writer.close()` drives the stream task through
    /// `WritableSink::close`, which is where transports flush their internal
    /// buffers (e.g. `BufWriter` → disk). Simply dropping the writer would
    /// cut the stream task off mid-queue without ever calling `close()`.
    async fn close_all(&mut self) {
        let drained: Vec<TransportSlot> = self.slots.drain(..).collect();
        let closes = drained.iter().map(|slot| async move {
            let _ = slot.writer.close().await;
        });
        let _ = futures::future::join_all(closes).await;
    }
}


/// The fanout task.
///
/// One async task per Logger consumes every `PipelineMessage`, applies
/// per-slot formatting and level filtering, and fans entries into each
/// transport's `WritableStream` writer concurrently. There is no
/// per-transport receive task — each transport's only async work is its
/// stream's pump task.
///
/// # Flush semantics
///
/// WHATWG streams have no mid-life flush primitive — only `close`. By the
/// time a `Flush` message reaches us through the pipeline channel, every
/// prior `writer.write(...).await` has resolved, meaning each transport's
/// stream has *received* every queued entry. Whether the sink's *internal*
/// buffer (e.g. `BufWriter`) has hit disk is up to the sink. The flush is
/// acked eagerly here; durable flush happens at transport teardown, when
/// `writer.close()` runs `WritableSink::close`.
pub async fn run_fanout(
    mut rx: fmpsc::UnboundedReceiver<PipelineMessage>,
    spawn_fn: SpawnFn,
    global_format: Option<Arc<dyn Format<Input = LogInfo> + Send + Sync>>,
    global_level: Option<String>,
    levels: Option<LoggerLevels>,
    buffer: Arc<Mutex<VecDeque<Arc<LogInfo>>>>,
    initial_transports: Vec<(TransportHandle, LoggerTransport)>,
) {
    let mut state = FanoutState {
        spawn_fn,
        slots: Vec::new(),
        global_format,
        global_level,
        levels,
        buffer,
    };
    for (handle, transport) in initial_transports {
        state.admit(handle, transport);
    }

    while let Some(msg) = rx.next().await {
        match msg {
            PipelineMessage::Entry(entry) => state.process_entry(entry).await,

            PipelineMessage::Flush(fc) => {
                let (lock, cvar) = &*fc;
                let mut done = lock.lock().unwrap();
                *done = true;
                cvar.notify_one();
            }

            PipelineMessage::AddTransport { handle, transport } => {
                state.admit(handle, transport);
                state.drain_buffer().await;
            }

            PipelineMessage::RemoveTransport(handle) => {
                if let Some(pos) = state.slots.iter().position(|s| s.handle == handle) {
                    let slot = state.slots.remove(pos);
                    let _ = slot.writer.close().await;
                }
            }

            PipelineMessage::Reconfigure {
                format,
                level,
                levels,
            } => {
                state.global_format = format;
                state.global_level = level;
                state.levels = levels;
            }

            PipelineMessage::Configure {
                format,
                level,
                levels,
                transports,
            } => {
                state.close_all().await;
                state.global_format = format;
                state.global_level = level;
                state.levels = levels;
                for (handle, transport) in transports {
                    state.admit(handle, transport);
                }
                state.drain_buffer().await;
            }

            PipelineMessage::Shutdown => {
                state.close_all().await;
            }
        }
    }

    // Pipeline channel closed: ensure every transport drains and closes its
    // sink before this task exits.
    state.close_all().await;
}


/// Builds and returns the pipeline channel sender.
///
/// Spawns exactly one fanout task. All other tasks are spawned indirectly,
/// one per transport, when a `WritableStream` is built inside the fanout —
/// no extra plumbing tasks.
pub fn build_pipeline(
    options: &LoggerOptions,
    buffer: Arc<Mutex<VecDeque<Arc<LogInfo>>>>,
    spawn_fn: SpawnFn,
) -> fmpsc::UnboundedSender<PipelineMessage> {
    let (tx, rx) = fmpsc::unbounded::<PipelineMessage>();

    let global_format = options.format.clone();
    let global_level = options.level.clone();
    let levels = options.levels.clone();
    let initial_transports = options.transports.clone().unwrap_or_default();

    let fanout_spawn = Arc::clone(&spawn_fn);
    spawn_fn(Box::pin(run_fanout(
        rx,
        fanout_spawn,
        global_format,
        global_level,
        levels,
        buffer,
        initial_transports,
    )));

    tx
}
