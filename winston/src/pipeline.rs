use std::{
    collections::{HashMap, VecDeque},
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex, RwLock,
    },
};

use futures::channel::{mpsc as fmpsc, oneshot};
use futures::StreamExt;
use logform::{FormatPipeline, FormattedEntry, LogInfo};
use whatwg_streams::{
    CountQueuingStrategy, StreamResult, WritableSink, WritableStream,
    WritableStreamDefaultWriter,
};
use winston_transport::{Transport, TransportSink};

use crate::{
    logger::TransportHandle,
    logger_levels::LoggerLevels,
    logger_options::OverflowPolicy,
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

/// The built-in spawner: runs each future on its own OS thread. Used
/// automatically when no spawner is provided.
///
/// Simple and isolating — a transport that blocks in `poll` can't stall any
/// other. But each transport runs two tasks (pump + WS controller), so a logger
/// with many transports spawns many OS threads and oversubscribes the scheduler:
/// fan-out cost scales super-linearly past ~4 transports (see
/// `docs/spawner-oversubscription-investigation.md`). For many transports, prefer
/// [`pooled_spawner`] (bounded threads, tolerates blocking) or
/// [`single_threaded_spawner`] (fastest, non-blocking sinks only).
pub fn default_spawner() -> SpawnFn {
    Arc::new(|fut: Pin<Box<dyn Future<Output = ()> + Send + 'static>>| {
        std::thread::spawn(move || {
            futures::executor::block_on(fut);
        });
    })
}

type SpawnedFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// Spawn one OS thread running a cooperative executor (a `FuturesUnordered`
/// pumped by an mpsc channel) and return the sender that feeds futures to it.
/// Shared by [`single_threaded_spawner`] and [`pooled_spawner`].
fn spawn_cooperative_executor(name: String) -> fmpsc::UnboundedSender<SpawnedFuture> {
    let (tx, rx) = fmpsc::unbounded::<SpawnedFuture>();
    std::thread::Builder::new()
        .name(name)
        .spawn(move || {
            futures::executor::block_on(async move {
                let mut rx = rx;
                let mut tasks = futures::stream::FuturesUnordered::<SpawnedFuture>::new();
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
    tx
}

/// Single-threaded spawner: every spawned future runs on one shared OS thread,
/// multiplexed cooperatively via a `FuturesUnordered` set.
///
/// The fastest option for fully async / non-blocking transports — it also keeps
/// each transport's pump and WS controller on the same thread, so their handoff
/// costs no cross-thread wakeup. But every task shares the one thread, so a
/// transport that performs synchronous blocking I/O inside `poll` stalls every
/// other task — pick [`default_spawner`] or [`pooled_spawner`] in that case.
pub fn single_threaded_spawner() -> SpawnFn {
    let tx = spawn_cooperative_executor("winston-executor".into());
    Arc::new(move |fut| {
        let _ = tx.unbounded_send(fut);
    })
}

/// Bounded-pool spawner: `n` cooperative executor threads, with spawned futures
/// distributed round-robin.
///
/// Caps total threads at `n` regardless of transport count, so it does **not**
/// oversubscribe the scheduler the way [`default_spawner`] does at many
/// transports — while a transport that blocks in `poll` stalls only the tasks
/// sharing its worker, not all of them (unlike [`single_threaded_spawner`]). The
/// scalable middle ground: prefer it for loggers with many transports,
/// especially a mix of blocking and non-blocking sinks.
///
/// `n` is clamped to at least 1; a good value is roughly the core count. See
/// `docs/spawner-oversubscription-investigation.md`.
pub fn pooled_spawner(n: usize) -> SpawnFn {
    let n = n.max(1);
    let senders: Vec<_> = (0..n)
        .map(|i| spawn_cooperative_executor(format!("winston-executor-{i}")))
        .collect();
    let next = std::sync::atomic::AtomicUsize::new(0);
    Arc::new(move |fut| {
        let idx = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % n;
        let _ = senders[idx].unbounded_send(fut);
    })
}


/// Sink-type-erased view of a `WritableStreamDefaultWriter<FormattedEntry, _>`.
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
        entry: FormattedEntry,
    ) -> Pin<Box<dyn Future<Output = StreamResult<()>> + Send + 'a>>;

    fn flush<'a>(&'a self) -> Pin<Box<dyn Future<Output = StreamResult<()>> + Send + 'a>>;

    fn close<'a>(&'a self) -> Pin<Box<dyn Future<Output = StreamResult<()>> + Send + 'a>>;
}

impl<Sink> ErasedWriter for WritableStreamDefaultWriter<FormattedEntry, Sink>
where
    Sink: WritableSink<FormattedEntry> + Send + Sync + 'static,
{
    fn enqueue_when_ready<'a>(
        &'a self,
        entry: FormattedEntry,
    ) -> Pin<Box<dyn Future<Output = StreamResult<()>> + Send + 'a>> {
        Box::pin(WritableStreamDefaultWriter::enqueue_when_ready(self, entry))
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

/// Default per-transport mailbox capacity — the deep buffer that bears the
/// slot's [`OverflowPolicy`]. Used when the transport doesn't override it via
/// `LoggerTransport::with_queue_capacity`.
pub const DEFAULT_TRANSPORT_QUEUE_CAPACITY: usize = 1024;

/// Default WritableStream high-water mark for a slot — a fixed cache-fit
/// constant rather than the mailbox capacity (a transport may override it via
/// `LoggerTransport::with_ws_high_water_mark`). The WS queue is the
/// pump↔controller throttle, not a policy buffer, and its sustained-throughput
/// optimum is an absolute count ≈ `L1 / sizeof(FormattedEntry)` — independent of
/// capacity. A shallow WS keeps the pump→controller handoff cache-warm; a deep
/// one makes the controller read cold, evicted data. Capped below by the mailbox
/// so a tiny `queue_capacity` is never out-deepened. See ADR 0007 and
/// `docs/queue-depth-investigation.md`.
pub(crate) const DEFAULT_WS_HWM: usize = 64;

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
        let stream = WritableStream::builder(TransportSink(transport))
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


/// Message sent from the dispatch path to a slot's pump task.
pub(crate) enum SlotMessage {
    /// A fully-rendered entry (format applied on the caller at dispatch).
    Entry(FormattedEntry),
    /// Barrier: pump processes prior `Entry` messages, drains until the
    /// stream is ready, then acks. Used to implement logger-level flush.
    Flush(oneshot::Sender<()>),
    /// Pump runs `writer.close()` (driving `WritableSink::close`), then exits.
    Close,
}

/// One per registered transport. Lives inside `Vec<Arc<TransportSlot>>` so
/// the Logger can hand out shared snapshots for sync dispatch without
/// holding the slot-list lock across a potentially-blocking push.
pub(crate) struct TransportSlot {
    pub(crate) handle: TransportHandle,
    pub(crate) level: Option<String>,
    pub(crate) transport_format: Option<Arc<FormatPipeline>>,
    pub(crate) overflow: OverflowPolicy,
    /// Single-producer mailbox into the per-slot pump. `MailboxSender`'s
    /// `&self` API lets every caller push through an `Arc<TransportSlot>`
    /// without external locking; the internal `Mutex<VecDeque>` enforces
    /// ordering.
    pub(crate) mailbox_tx: MailboxSender<SlotMessage>,
    /// Resolves when the pump task has exited (after `writer.close()`).
    /// `Mutex<Option<…>>` so admin paths can `.take()` it via a shared
    /// reference to the slot.
    pub(crate) pump_done: parking_lot::Mutex<Option<oneshot::Receiver<()>>>,
    /// Same `Arc` the Logger reads from for `transport_stats(handle)`.
    pub(crate) stats: Arc<TransportStatsInner>,
    /// Edge-trigger state for `BackpressureEvent`. The dispatch path is
    /// not internally synchronised across concurrent producers, but the
    /// swap on every push gives "at least one Saturated per transition"
    /// — duplicates are bounded by concurrent producers, not unbounded.
    pub(crate) was_saturated: AtomicBool,
}

/// The per-transport pump task.
///
/// Owns the slot's writer and drains the mailbox in order. For each entry it
/// runs the finalizer (the render the caller deferred), then hands the
/// resulting `FormattedEntry` to `enqueue_when_ready` — the pump parks when the
/// WritableStream's queue hits its high-water mark, so chunks pipeline through
/// the sink instead of being serialised on completion. Rendering here keeps it
/// off the caller's thread, parallel across transports, and skipped for any
/// entry dropped at the mailbox boundary.
///
/// `Flush` translates to `writer.flush()`, which awaits every queued and
/// in-flight chunk against the sink without tearing the stream down. The
/// pump is policy-agnostic — `OverflowPolicy` only governs the dispatch path's
/// behaviour on a full mailbox.
async fn slot_pump(
    mut mailbox_rx: MailboxReceiver<SlotMessage>,
    writer: Box<dyn ErasedWriter>,
    done: oneshot::Sender<()>,
) {
    while let Some(msg) = mailbox_rx.next().await {
        match msg {
            SlotMessage::Entry(entry) => {
                let _ = writer.enqueue_when_ready(entry).await;
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


/// The WritableStream high-water mark for a slot. An `explicit` override
/// (`LoggerTransport::with_ws_high_water_mark`) wins as-is; otherwise the fixed
/// cache-fit constant [`DEFAULT_WS_HWM`], capped below by the mailbox `capacity`
/// so a tiny capacity is never out-deepened by the WS. Under the
/// `internal-bench` feature a `WINSTON_WS_HWM` env var forces it, so the
/// `queue_depth` benchmark can sweep the WS depth independently of the mailbox;
/// no effect in normal builds.
fn resolve_ws_hwm(capacity: usize, explicit: Option<usize>) -> usize {
    #[cfg(feature = "internal-bench")]
    if let Some(n) = std::env::var("WINSTON_WS_HWM")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
    {
        return n.max(1);
    }
    match explicit {
        Some(n) => n.max(1),
        None => capacity.min(DEFAULT_WS_HWM),
    }
}

/// Build one slot from a `LoggerTransport`: spawn its pump on `spawn_fn`, and
/// wire its two bounded buffers — the mailbox at the transport's `queue_capacity`
/// (sync front door, bearing the slot's `OverflowPolicy`) and the
/// `WritableStream` at [`resolve_ws_hwm`] (the shallow pump↔controller throttle).
/// Returns `None` if the transport's writer builder was already consumed (e.g. a
/// duplicate add) — that slot is silently skipped.
pub(crate) fn build_slot(
    spawn_fn: &SpawnFn,
    stats_map: &TransportStatsMap,
    handle: TransportHandle,
    transport: LoggerTransport,
) -> Option<Arc<TransportSlot>> {
    let level = transport.get_level().cloned();
    let transport_format = transport.get_format();
    let overflow = transport.overflow_policy();
    let capacity = transport.queue_capacity();
    let ws_hwm = resolve_ws_hwm(capacity, transport.ws_high_water_mark());
    let builder = transport.take_builder()?;
    let writer = builder(Arc::clone(spawn_fn), ws_hwm);

    let stats = stats_map
        .write()
        .unwrap()
        .entry(handle)
        .or_insert_with(|| Arc::new(TransportStatsInner::default()))
        .clone();

    let (mailbox_tx, mailbox_rx) = mailbox::channel::<SlotMessage>(capacity);
    let (done_tx, done_rx) = oneshot::channel();
    let pump = slot_pump(mailbox_rx, writer, done_tx);
    spawn_fn(Box::pin(pump));

    Some(Arc::new(TransportSlot {
        handle,
        level,
        transport_format,
        overflow,
        mailbox_tx,
        pump_done: parking_lot::Mutex::new(Some(done_rx)),
        stats,
        was_saturated: AtomicBool::new(false),
    }))
}

/// Immutable routing state for the hot dispatch path. Held by `Logger`
/// behind an `ArcSwap`: `log()` does a single lock-free `load_full`, then
/// dispatches against the loaded `Routing`. A `Block`-policy park holds only
/// this `Arc`, never a lock, so a parked producer can't stall admin
/// operations. Admin operations rebuild a `Routing` and `store` it under a
/// serialising mutex.
pub(crate) struct Routing {
    pub(crate) slots: Vec<Arc<TransportSlot>>,
    pub(crate) global_format: Option<Arc<FormatPipeline>>,
    pub(crate) global_level: Option<String>,
    pub(crate) levels: Option<LoggerLevels>,
    /// Entries logged before any transport was admitted. Drained into the
    /// slots the moment the first transport arrives.
    pub(crate) buffer: Arc<Mutex<VecDeque<Arc<LogInfo>>>>,
    pub(crate) event_senders: EventSenders,
}

impl Routing {
    pub(crate) fn new(
        slots: Vec<Arc<TransportSlot>>,
        global_format: Option<Arc<FormatPipeline>>,
        global_level: Option<String>,
        levels: Option<LoggerLevels>,
        buffer: Arc<Mutex<VecDeque<Arc<LogInfo>>>>,
        event_senders: EventSenders,
    ) -> Self {
        Self {
            slots,
            global_format,
            global_level,
            levels,
            buffer,
            event_senders,
        }
    }

    /// Clone the routing with a different slot list, reusing the (small)
    /// global format/level/levels and the shared buffer/event_senders. Used
    /// by `add`/`remove`/`close` to publish a new slot list.
    pub(crate) fn with_slots(&self, slots: Vec<Arc<TransportSlot>>) -> Self {
        Self {
            slots,
            global_format: self.global_format.clone(),
            global_level: self.global_level.clone(),
            levels: self.levels.clone(),
            buffer: Arc::clone(&self.buffer),
            event_senders: Arc::clone(&self.event_senders),
        }
    }

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

    /// Stage 2 of the two-pass format: compose the transport's format on top of
    /// the already-global-*transformed* `enriched` entry and finalize it for one
    /// slot (ADR 0008). The global *transforms* have already run once, in
    /// `dispatch_entry`'s stage 1; only the transport-specific work happens here.
    ///
    /// - **Transport has a format** → run its transforms over `enriched`, then its
    ///   finalizer (authoritative, `None` included). Returns `None` if a transport
    ///   transform drops the entry.
    /// - **No transport format** → inherit the global finalizer (its transforms
    ///   already ran); with no global finalizer either, passthrough.
    fn format_for(&self, slot: &TransportSlot, enriched: &LogInfo) -> Option<FormattedEntry> {
        match &slot.transport_format {
            Some(tf) => Some(tf.finalize(tf.transform(enriched.clone())?)),
            None => match &self.global_format {
                Some(gf) => Some(gf.finalize(enriched.clone())),
                None => Some(FormattedEntry::new(enriched.clone(), None)),
            },
        }
    }

    /// Synchronously dispatch one entry across every slot.
    ///
    /// Stage 1 (once): run the **global transforms** a single time, producing one
    /// enriched entry shared by every slot — so a time- or state-dependent global
    /// transform (`timestamp`, a sampling filter) is evaluated once and all
    /// transports observe the same result (ADR 0008, dedup Layer 1). A global
    /// transform that drops the entry skips every slot.
    ///
    /// Then two phases over the slots:
    ///
    /// 1. Compose the transport stage on the enriched entry (`format_for`) and
    ///    non-blocking `try_push` to every slot whose level admits the entry —
    ///    level-gated on the *original* entry level, before any transform. Drop
    ///    policies (`DropNewest` / `DropOldest`) resolve here; only `Block`-policy
    ///    slots whose mailbox is full are deferred.
    ///
    /// 2. For each deferred `Block` slot, `push_blocking` parks the
    ///    calling thread until that slot has room. Other transports are
    ///    *not* gated on this wait — they've already received the entry
    ///    in phase 1. This is the "water flows to every pipe; only the
    ///    blocked pipe ripples upstream" model from ADR 0002.
    ///
    /// The worker visits deferred slots in order, but the per-slot
    /// pumps run on independent tasks — by the time the worker parks
    /// on slot B, slot B's pump has been draining concurrently with
    /// A's pump the whole time. Total caller wait is `max(drain_times)`,
    /// not `sum`. ADR 0003 walks the trace and records why the
    /// `block_on(join_all(...))` "parallel waiting" alternative was
    /// considered and rejected (adds executor cost without a
    /// corresponding semantic improvement).
    pub(crate) fn dispatch_entry(&self, entry: &Arc<LogInfo>) {
        // Stage 1: global transforms, once. Drop here skips every slot.
        let enriched: Arc<LogInfo> = match &self.global_format {
            Some(gf) => match gf.transform((**entry).clone()) {
                Some(transformed) => Arc::new(transformed),
                None => return,
            },
            None => Arc::clone(entry),
        };

        let mut deferred_blocks: Vec<(usize, SlotMessage)> = Vec::new();

        // Phase 1: compose the transport stage on `enriched`, try-push.
        for (idx, slot) in self.slots.iter().enumerate() {
            if !self.passes_level(&entry.level, slot.level.as_ref()) {
                continue;
            }
            let Some(formatted) = self.format_for(slot, &enriched) else {
                continue;
            };
            if let Some(deferred) = self.try_push_to_slot(slot, SlotMessage::Entry(formatted)) {
                deferred_blocks.push((idx, deferred));
            }
        }

        // Phase 2: block the caller for each Block-saturated slot.
        for (idx, msg) in deferred_blocks {
            let slot = &self.slots[idx];
            if slot.mailbox_tx.push_blocking(msg).is_ok() {
                slot.stats.dispatched_total.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Async twin of [`dispatch_entry`]. Phase 1 is identical — non-blocking
    /// `try_push` to every admitted slot, with drop policies resolved inline.
    /// Phase 2 **awaits** each `Block`-saturated slot's mailbox (yielding the
    /// task) instead of parking the caller's thread, and `join`s those waits so
    /// a slow slot does not serialise the rest (ADR 0003's parallelism, ADR
    /// 0009's cooperative backpressure).
    ///
    /// [`dispatch_entry`]: Routing::dispatch_entry
    #[cfg(feature = "async-log")]
    pub(crate) async fn dispatch_entry_async(&self, entry: &Arc<LogInfo>) {
        // Stage 1: global transforms, once. Drop here skips every slot.
        let enriched: Arc<LogInfo> = match &self.global_format {
            Some(gf) => match gf.transform((**entry).clone()) {
                Some(transformed) => Arc::new(transformed),
                None => return,
            },
            None => Arc::clone(entry),
        };

        let mut deferred_blocks: Vec<(usize, SlotMessage)> = Vec::new();

        // Phase 1: identical to the sync path.
        for (idx, slot) in self.slots.iter().enumerate() {
            if !self.passes_level(&entry.level, slot.level.as_ref()) {
                continue;
            }
            let Some(formatted) = self.format_for(slot, &enriched) else {
                continue;
            };
            if let Some(deferred) = self.try_push_to_slot(slot, SlotMessage::Entry(formatted)) {
                deferred_blocks.push((idx, deferred));
            }
        }

        if deferred_blocks.is_empty() {
            return;
        }

        // Phase 2: await room per Block-saturated slot, in parallel. Each future
        // owns an `Arc` clone of its slot, so nothing borrows `self` across the
        // await.
        let sends = deferred_blocks.into_iter().map(|(idx, msg)| {
            let slot = Arc::clone(&self.slots[idx]);
            async move {
                if slot.mailbox_tx.send(msg).await.is_ok() {
                    slot.stats.dispatched_total.fetch_add(1, Ordering::Relaxed);
                }
            }
        });
        futures::future::join_all(sends).await;
    }

    /// Non-blocking push of a prepared `Entry` message to a single slot.
    /// Returns `Some(msg)` only when the slot is `Block`-policy and the
    /// mailbox is full — caller must then call `push_blocking` in phase 2.
    /// Returns `None` on any other outcome (success, drop policy, slot torn
    /// down) — all of which the caller treats as "done" for this slot.
    fn try_push_to_slot(&self, slot: &TransportSlot, msg: SlotMessage) -> Option<SlotMessage> {
        match slot.mailbox_tx.try_push(msg) {
            Ok(()) => {
                slot.stats.dispatched_total.fetch_add(1, Ordering::Relaxed);
                if slot.was_saturated.swap(false, Ordering::Relaxed) {
                    emit_event(
                        &self.event_senders,
                        BackpressureEvent::Recovered { handle: slot.handle },
                    );
                }
                None
            }
            Err(TryPushError::Full(msg)) => {
                if !slot.was_saturated.swap(true, Ordering::Relaxed) {
                    emit_event(
                        &self.event_senders,
                        BackpressureEvent::Saturated { handle: slot.handle },
                    );
                }
                match slot.overflow {
                    OverflowPolicy::Block => Some(msg),
                    OverflowPolicy::DropNewest => {
                        slot.stats.dropped_total.fetch_add(1, Ordering::Relaxed);
                        None
                    }
                    OverflowPolicy::DropOldest => {
                        let _evicted = slot.mailbox_tx.force_push_dropping_oldest(msg);
                        slot.stats.dispatched_total.fetch_add(1, Ordering::Relaxed);
                        slot.stats.dropped_total.fetch_add(1, Ordering::Relaxed);
                        None
                    }
                }
            }
            Err(TryPushError::Closed(_)) => None, // pump gone — slot torn down
        }
    }

    /// Drain entries that were logged before any transport was admitted.
    /// Called from the admin path after `admit` lands the first slot.
    pub(crate) fn drain_buffer_to_slots(&self) {
        let buffered: Vec<Arc<LogInfo>> = {
            let mut buf = self.buffer.lock().unwrap();
            buf.drain(..).collect()
        };
        for entry in &buffered {
            self.dispatch_entry(entry);
        }
    }

    /// Empty-message gate, no-transport buffering, dispatch. The full
    /// `Logger::log` happy-path runs through here.
    pub(crate) fn process_entry(&self, entry: Arc<LogInfo>) {
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

        self.dispatch_entry(&entry);
    }

    /// Async twin of [`process_entry`]: same empty-message gate and
    /// no-transport buffering; a saturated `Block` slot is awaited rather than
    /// blocked on. The `Logger::log_async` happy path runs through here.
    ///
    /// [`process_entry`]: Routing::process_entry
    #[cfg(feature = "async-log")]
    pub(crate) async fn process_entry_async(&self, entry: Arc<LogInfo>) {
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

        self.dispatch_entry_async(&entry).await;
    }

    /// Send `Flush` barriers into every slot, then block the calling
    /// thread on every pump's ack. Uses `push_blocking` (not `try_push`)
    /// so the barrier always lands, even when the mailbox is full.
    pub(crate) fn flush_all_sync(&self) {
        let mut acks = Vec::with_capacity(self.slots.len());
        for slot in &self.slots {
            let (tx, rx) = oneshot::channel();
            if slot.mailbox_tx.push_blocking(SlotMessage::Flush(tx)).is_ok() {
                acks.push(rx);
            }
        }
        if acks.is_empty() {
            return;
        }
        futures::executor::block_on(async move {
            for rx in acks {
                let _ = rx.await;
            }
        });
    }
}

/// Tear down every slot: send `Close`, then join every pump in parallel.
///
/// Sends are serialised (sync `push_blocking`), but each `push_blocking`
/// is brief — the mailbox is bounded and Close is one message. The
/// parallel piece that matters is awaiting `pump_done`: each pump runs
/// `writer.close()` (which drives `WritableSink::close`), and one slow
/// transport shouldn't compound shutdown latency for the others.
pub(crate) fn close_slots_sync(
    slots: Vec<Arc<TransportSlot>>,
    stats_map: &TransportStatsMap,
) {
    if slots.is_empty() {
        return;
    }
    let mut handles = Vec::with_capacity(slots.len());
    let mut dones = Vec::with_capacity(slots.len());
    for slot in &slots {
        handles.push(slot.handle);
        let _ = slot.mailbox_tx.push_blocking(SlotMessage::Close);
        if let Some(rx) = slot.pump_done.lock().take() {
            dones.push(rx);
        }
    }
    if !dones.is_empty() {
        futures::executor::block_on(async move {
            let _ = futures::future::join_all(dones).await;
        });
    }
    let mut map = stats_map.write().unwrap();
    for h in handles {
        map.remove(&h);
    }
}
