use std::{
    collections::{HashMap, VecDeque},
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Condvar, Mutex, RwLock,
    },
};

use futures::channel::{mpsc as fmpsc, oneshot};
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
    logger_options::{LoggerOptions, OverflowPolicy},
    logger_transport::LoggerTransport,
    mailbox::{self, MailboxReceiver, MailboxSender, TryPushError},
};

/// Snapshot of a transport's lifetime counters.
///
/// Returned by [`Logger::transport_stats`](crate::Logger::transport_stats).
/// All fields are monotonically non-decreasing — instantaneous queue depth
/// is not (yet) exposed; derive throughput / drop rate by sampling.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TransportStats {
    /// Entries the fanout successfully handed to the slot's mailbox.
    pub dispatched_total: u64,
    /// Entries dropped at the mailbox boundary because the mailbox was
    /// full and the slot's [`OverflowPolicy`] selected a drop variant.
    pub dropped_total: u64,
}

#[derive(Default)]
pub(crate) struct TransportStatsInner {
    dispatched_total: AtomicU64,
    dropped_total: AtomicU64,
}

impl TransportStatsInner {
    pub(crate) fn snapshot(&self) -> TransportStats {
        TransportStats {
            dispatched_total: self.dispatched_total.load(Ordering::Relaxed),
            dropped_total: self.dropped_total.load(Ordering::Relaxed),
        }
    }
}

/// Shared map between the Logger (read-only consumer) and the fanout task
/// (which inserts on admit, removes on tear-down). Cheap reads via
/// `Arc::clone` on the inner — counter loads happen against the cloned
/// `TransportStatsInner` without holding the map lock.
pub(crate) type TransportStatsMap =
    Arc<RwLock<HashMap<TransportHandle, Arc<TransportStatsInner>>>>;

/// Edge-triggered notification of per-transport backpressure transitions.
///
/// Subscribe via [`Logger::subscribe_backpressure`](crate::Logger::subscribe_backpressure).
/// Emitted only when a slot's mailbox crosses the empty↔full boundary —
/// not per-entry — so the event rate stays bounded even under sustained
/// pressure. Pair with [`TransportStats`] for steady-state numbers.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum BackpressureEvent {
    /// The slot's mailbox transitioned from has-room to full. Subsequent
    /// dispatches will trigger the slot's [`OverflowPolicy`].
    Saturated { handle: TransportHandle },
    /// The slot's mailbox transitioned from full back to has-room.
    Recovered { handle: TransportHandle },
}

/// Multi-subscriber broadcast list for `BackpressureEvent`. The fanout
/// prunes disconnected senders on each emit, so dropping the receiver is
/// the only "unsubscribe" needed.
pub(crate) type EventSenders =
    Arc<Mutex<Vec<fmpsc::UnboundedSender<BackpressureEvent>>>>;

fn emit_event(senders: &EventSenders, event: BackpressureEvent) {
    let mut s = senders.lock().unwrap();
    s.retain(|tx| tx.unbounded_send(event.clone()).is_ok());
}

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
/// Each transport has a different concrete `Sink` type, so the slot pump
/// can't store its writer behind a homogeneous interface directly. The
/// pump needs:
/// - `enqueue_when_ready` — fire chunks into the stream queue (bounded by
///   HWM via `ready()`), so multiple writes can pipeline through the sink.
/// - `flush` — wait until every queued + in-flight chunk has been processed
///   by the sink, without closing the stream.
/// - `close` — drive `WritableSink::close` at teardown.
pub(crate) trait ErasedWriter: Send + Sync {
    fn enqueue_when_ready<'a>(
        &'a self,
        info: LogInfo,
    ) -> Pin<Box<dyn Future<Output = StreamResult<()>> + Send + 'a>>;

    fn flush<'a>(&'a self) -> Pin<Box<dyn Future<Output = StreamResult<()>> + Send + 'a>>;

    fn close<'a>(&'a self) -> Pin<Box<dyn Future<Output = StreamResult<()>> + Send + 'a>>;
}

impl<Sink> ErasedWriter for WritableStreamDefaultWriter<LogInfo, Sink>
where
    Sink: WritableSink<LogInfo> + Send + Sync + 'static,
{
    fn enqueue_when_ready<'a>(
        &'a self,
        info: LogInfo,
    ) -> Pin<Box<dyn Future<Output = StreamResult<()>> + Send + 'a>> {
        Box::pin(WritableStreamDefaultWriter::enqueue_when_ready(self, info))
    }

    fn flush<'a>(&'a self) -> Pin<Box<dyn Future<Output = StreamResult<()>> + Send + 'a>> {
        Box::pin(WritableStreamDefaultWriter::flush(self))
    }

    fn close<'a>(&'a self) -> Pin<Box<dyn Future<Output = StreamResult<()>> + Send + 'a>> {
        Box::pin(WritableStreamDefaultWriter::close(self))
    }
}

/// One-shot factory the fanout invokes when admitting a transport.
///
/// Captures the typed transport. The fanout supplies the `SpawnFn` and the
/// per-transport queue high-water mark at call time (the latter taken from
/// the `LoggerTransport`'s `queue_capacity`), spawns the `WritableStream`
/// pump task, and returns the sink-erased writer.
pub type TransportWriterBuilder =
    Box<dyn FnOnce(SpawnFn, usize) -> Box<dyn ErasedWriter> + Send>;

/// Default per-transport queue capacity. Applied to both the slot mailbox
/// and the WritableStream's high-water mark when the transport doesn't
/// override it via `LoggerTransport::with_queue_capacity`.
pub const DEFAULT_TRANSPORT_QUEUE_CAPACITY: usize = 1024;

/// Construct the type-erased builder used by `LoggerTransport`.
///
/// Wraps the typed transport in a `WritableStream` (whose pump task runs on
/// the spawner supplied at admission time), takes its writer, and erases the
/// Sink type parameter via `ErasedWriter`.
pub(crate) fn make_writer_builder<T>(transport: T) -> TransportWriterBuilder
where
    T: Transport,
{
    Box::new(move |spawn_fn: SpawnFn, hwm: usize| {
        let stream = WritableStream::builder(transport)
            .strategy(CountQueuingStrategy::new(hwm))
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


/// Message sent from the fanout to a slot's pump task.
enum SlotMessage {
    Entry(LogInfo),
    /// Barrier: pump processes prior `Entry` messages, drains until the
    /// stream is ready, then acks. Used to implement logger-level flush.
    Flush(oneshot::Sender<()>),
    /// Pump runs `writer.close()` (driving `WritableSink::close`), then exits.
    Close,
}

struct TransportSlot {
    handle: TransportHandle,
    level: Option<String>,
    transport_format: Option<Arc<dyn Format<Input = LogInfo> + Send + Sync>>,
    overflow: OverflowPolicy,
    /// Bounded channel into the per-slot pump. Fanout dispatches via
    /// `try_send` here; on full, `overflow` decides what happens next.
    mailbox_tx: MailboxSender<SlotMessage>,
    /// Resolves when the pump task has exited (after `writer.close()`).
    pump_done: Option<oneshot::Receiver<()>>,
    /// Same `Arc` the Logger reads from for `transport_stats(handle)`.
    stats: Arc<TransportStatsInner>,
    /// Edge-trigger state for `BackpressureEvent`. Single-writer (fanout
    /// task) but stored atomic so the slot can be borrowed `&`.
    was_saturated: AtomicBool,
}

/// The per-transport pump task.
///
/// Owns the slot's writer and drains the mailbox in order. Entries go
/// through `enqueue_when_ready` — the pump parks when the WritableStream's
/// queue hits its high-water mark, so the HWM does real work and chunks
/// pipeline through the sink instead of being serialised on completion.
///
/// `Flush` translates to `writer.flush()`, which awaits every queued and
/// in-flight chunk against the sink without tearing the stream down. The
/// pump is policy-agnostic — `OverflowPolicy` only governs the fanout's
/// dispatch on a full mailbox.
async fn slot_pump(
    mut mailbox_rx: MailboxReceiver<SlotMessage>,
    writer: Box<dyn ErasedWriter>,
    done: oneshot::Sender<()>,
) {
    while let Some(msg) = mailbox_rx.next().await {
        match msg {
            SlotMessage::Entry(info) => {
                let _ = writer.enqueue_when_ready(info).await;
            }
            SlotMessage::Flush(ack) => {
                let _ = writer.flush().await;
                let _ = ack.send(());
            }
            SlotMessage::Close => {
                let _ = writer.close().await;
                break;
            }
        }
    }
    let _ = done.send(());
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
    /// Per-transport counter map, shared read-only with the Logger.
    stats_map: TransportStatsMap,
    /// Active `BackpressureEvent` subscribers. Cloned senders; dead ones
    /// pruned on emit.
    event_senders: EventSenders,
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
        let overflow = transport.overflow_policy();
        let capacity = transport.queue_capacity();
        let Some(builder) = transport.take_builder() else {
            return; // Already consumed (e.g. duplicate add) — ignore silently.
        };
        let writer = builder(Arc::clone(&self.spawn_fn), capacity);

        let stats = self
            .stats_map
            .write()
            .unwrap()
            .entry(handle)
            .or_insert_with(|| Arc::new(TransportStatsInner::default()))
            .clone();

        let (mailbox_tx, mailbox_rx) = mailbox::channel::<SlotMessage>(capacity);
        let (done_tx, done_rx) = oneshot::channel();
        let pump = slot_pump(mailbox_rx, writer, done_tx);
        (self.spawn_fn)(Box::pin(pump));

        self.slots.push(TransportSlot {
            handle,
            level,
            transport_format,
            overflow,
            mailbox_tx,
            pump_done: Some(done_rx),
            stats,
            was_saturated: AtomicBool::new(false),
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

    /// Dispatches an entry into each slot's mailbox according to its policy.
    ///
    /// `try_send` is non-blocking. On full, `Block` slots await `send` —
    /// which couples the fanout to this slot's drain rate (and via the
    /// fanout, every other slot for the duration), propagating pressure
    /// upstream. `DropNewest` slots drop the entry silently.
    ///
    /// Emits `Saturated` on the empty→full edge and `Recovered` on the
    /// next dispatch that sees room after a saturation. The Block path
    /// intentionally does not emit `Recovered` after `send().await` —
    /// freeing a single slot mid-pressure isn't recovery, and the next
    /// dispatch's `try_send` will catch it if it is.
    ///
    /// Uses `&mut` on the slot's owned sender — must NOT clone, because
    /// `fmpsc::channel`'s capacity is `buffer + num_senders` and every
    /// live clone would inflate the bound, defeating the mailbox cap.
    async fn fan_entry(&mut self, entry: &Arc<LogInfo>) {
        let event_senders = Arc::clone(&self.event_senders);
        for i in 0..self.slots.len() {
            // Phase 1: level / format checks with immutable borrows on
            // self (passes_level, format_for) and the slot.
            let (info_opt, overflow, handle) = {
                let slot = &self.slots[i];
                let info = if self.passes_level(&entry.level, slot.level.as_ref()) {
                    self.format_for(slot, entry)
                } else {
                    None
                };
                (info, slot.overflow, slot.handle)
            };
            let Some(info) = info_opt else { continue };

            // Phase 2: mutable borrow on the slot. The mailbox is a
            // single-producer custom queue with a fixed capacity — no
            // per-sender slot inflation; `try_push` honours the cap.
            let slot = &mut self.slots[i];
            let result = slot.mailbox_tx.try_push(SlotMessage::Entry(info));
            match result {
                Ok(()) => {
                    slot.stats.dispatched_total.fetch_add(1, Ordering::Relaxed);
                    if slot.was_saturated.swap(false, Ordering::Relaxed) {
                        emit_event(
                            &event_senders,
                            BackpressureEvent::Recovered { handle },
                        );
                    }
                }
                Err(TryPushError::Full(msg)) => {
                    if !slot.was_saturated.swap(true, Ordering::Relaxed) {
                        emit_event(
                            &event_senders,
                            BackpressureEvent::Saturated { handle },
                        );
                    }
                    match overflow {
                        OverflowPolicy::Block => {
                            if slot.mailbox_tx.send(msg).await.is_ok() {
                                slot.stats.dispatched_total.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        OverflowPolicy::DropNewest => {
                            slot.stats.dropped_total.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
                Err(TryPushError::Closed(_)) => {} // pump gone — slot torn down
            }
        }
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

    /// Send Flush barriers into every slot and await all acks.
    ///
    /// `send().await` (not `try_push`) guarantees the barrier reaches the
    /// back of each mailbox even when full — flush is a synchronisation
    /// point, not a drop candidate. Each pump processes the barrier after
    /// all prior `Entry` messages.
    async fn flush_all(&mut self) {
        let mut acks = Vec::with_capacity(self.slots.len());
        for slot in &mut self.slots {
            let (tx, rx) = oneshot::channel();
            if slot.mailbox_tx.send(SlotMessage::Flush(tx)).await.is_ok() {
                acks.push(rx);
            }
        }
        for rx in acks {
            let _ = rx.await;
        }
    }

    /// Tear down every slot: send `Close`, then join each pump.
    ///
    /// The pump runs `writer.close()` (which drives `WritableSink::close`)
    /// before exiting. Without joining, dropping the mailbox would race the
    /// pump's close future and skip the sink's flush-and-close lifecycle.
    async fn close_all(&mut self) {
        let drained: Vec<TransportSlot> = self.slots.drain(..).collect();
        let mut dones = Vec::with_capacity(drained.len());
        let mut handles = Vec::with_capacity(drained.len());
        for mut slot in drained {
            handles.push(slot.handle);
            let _ = slot.mailbox_tx.send(SlotMessage::Close).await;
            if let Some(rx) = slot.pump_done.take() {
                dones.push(rx);
            }
        }
        let _ = futures::future::join_all(dones).await;
        let mut map = self.stats_map.write().unwrap();
        for h in handles {
            map.remove(&h);
        }
    }
}


/// The fanout task.
///
/// One async task per Logger consumes every `PipelineMessage`, applies
/// per-slot formatting and level filtering, and dispatches entries into
/// each transport's per-slot mailbox. The mailbox is drained by a
/// per-transport pump task that owns the `WritableStream` writer.
///
/// # Pressure model
///
/// The fanout itself never blocks on `Entry` dispatch unless a slot is
/// configured `OverflowPolicy::Block` *and* its mailbox is full. In that
/// case the fanout awaits `send` on that slot's mailbox, which propagates
/// pressure back through the bridge → main channel → caller. Slots with
/// `DropNewest` short-circuit at the mailbox boundary and never gate the
/// fanout.
///
/// # Flush semantics
///
/// `Flush` is implemented as a per-slot barrier: a `SlotMessage::Flush`
/// is sent (via `send().await`) into every mailbox and the fanout awaits
/// every pump's ack. Each pump acks after `writer.ready().await` — i.e.
/// once its queue has drained below the high-water mark. Durable flush
/// (sink-level buffer → disk) happens at transport teardown via
/// `WritableSink::close`.
pub async fn run_fanout(
    mut rx: fmpsc::UnboundedReceiver<PipelineMessage>,
    spawn_fn: SpawnFn,
    global_format: Option<Arc<dyn Format<Input = LogInfo> + Send + Sync>>,
    global_level: Option<String>,
    levels: Option<LoggerLevels>,
    buffer: Arc<Mutex<VecDeque<Arc<LogInfo>>>>,
    initial_transports: Vec<(TransportHandle, LoggerTransport)>,
    stats_map: TransportStatsMap,
    event_senders: EventSenders,
) {
    let mut state = FanoutState {
        spawn_fn,
        slots: Vec::new(),
        global_format,
        global_level,
        levels,
        buffer,
        stats_map,
        event_senders,
    };
    for (handle, transport) in initial_transports {
        state.admit(handle, transport);
    }

    while let Some(msg) = rx.next().await {
        match msg {
            PipelineMessage::Entry(entry) => state.process_entry(entry).await,

            PipelineMessage::Flush(fc) => {
                state.flush_all().await;
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
                    let mut slot = state.slots.remove(pos);
                    let _ = slot.mailbox_tx.send(SlotMessage::Close).await;
                    if let Some(rx) = slot.pump_done.take() {
                        let _ = rx.await;
                    }
                    state.stats_map.write().unwrap().remove(&handle);
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
    stats_map: TransportStatsMap,
    event_senders: EventSenders,
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
        stats_map,
        event_senders,
    )));

    tx
}
