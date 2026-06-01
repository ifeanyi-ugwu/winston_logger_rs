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
use logform::{Format, LogInfo};
use whatwg_streams::{
    CountQueuingStrategy, StreamResult, WritableSink, WritableStream,
    WritableStreamDefaultWriter,
};
use winston_transport::Transport;

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

/// One per registered transport. Lives inside `Vec<Arc<TransportSlot>>` so
/// the Logger can hand out shared snapshots for sync dispatch without
/// holding the slot-list lock across a potentially-blocking push.
pub(crate) struct TransportSlot {
    pub(crate) handle: TransportHandle,
    pub(crate) level: Option<String>,
    pub(crate) transport_format: Option<Arc<dyn Format<Input = LogInfo> + Send + Sync>>,
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


/// The Logger's runtime state. Owned by `Logger` behind a
/// `parking_lot::RwLock`: `log()` takes a read lock, snapshots the slot
/// list (cheap `Arc` clones), and dispatches *without* holding the lock,
/// so a `Block`-policy `push_blocking` parking the producer doesn't
/// stall admin operations. Admin operations (add/remove/configure/close)
/// take the write lock.
pub(crate) struct LoggerState {
    pub(crate) spawn_fn: SpawnFn,
    pub(crate) slots: Vec<Arc<TransportSlot>>,
    pub(crate) global_format: Option<Arc<dyn Format<Input = LogInfo> + Send + Sync>>,
    pub(crate) global_level: Option<String>,
    pub(crate) levels: Option<LoggerLevels>,
    /// Entries logged before any transport was admitted. Drained into the
    /// slots the moment the first transport arrives.
    pub(crate) buffer: Arc<Mutex<VecDeque<Arc<LogInfo>>>>,
    pub(crate) stats_map: TransportStatsMap,
    pub(crate) event_senders: EventSenders,
}

impl LoggerState {
    pub(crate) fn new(
        spawn_fn: SpawnFn,
        global_format: Option<Arc<dyn Format<Input = LogInfo> + Send + Sync>>,
        global_level: Option<String>,
        levels: Option<LoggerLevels>,
        buffer: Arc<Mutex<VecDeque<Arc<LogInfo>>>>,
        stats_map: TransportStatsMap,
        event_senders: EventSenders,
    ) -> Self {
        Self {
            spawn_fn,
            slots: Vec::new(),
            global_format,
            global_level,
            levels,
            buffer,
            stats_map,
            event_senders,
        }
    }

    /// Build a slot from a `LoggerTransport` and push it onto the list.
    /// Caller is responsible for triggering `drain_buffer_locked` after
    /// admit if this is the first slot (so pre-transport entries land).
    pub(crate) fn admit(
        &mut self,
        handle: TransportHandle,
        transport: LoggerTransport,
    ) {
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

        self.slots.push(Arc::new(TransportSlot {
            handle,
            level,
            transport_format,
            overflow,
            mailbox_tx,
            pump_done: parking_lot::Mutex::new(Some(done_rx)),
            stats,
            was_saturated: AtomicBool::new(false),
        }));
    }

    /// Snapshot the slot list and global format/level/levels. Cheap (Arc
    /// clones); held *only* during the snapshot itself, then released so
    /// dispatch happens lock-free.
    pub(crate) fn snapshot(&self) -> StateSnapshot {
        StateSnapshot {
            slots: self.slots.clone(),
            global_format: self.global_format.clone(),
            global_level: self.global_level.clone(),
            levels: self.levels.clone(),
            buffer: Arc::clone(&self.buffer),
            event_senders: Arc::clone(&self.event_senders),
        }
    }

    /// Take Close-receivers for the current slots so `Drop` can join them
    /// without holding the state lock. Used during shutdown when the
    /// caller already owns `&mut self`.
    pub(crate) fn drain_slots(&mut self) -> Vec<Arc<TransportSlot>> {
        std::mem::take(&mut self.slots)
    }
}

/// Read-only snapshot of `LoggerState` taken under a brief lock. All
/// dispatch and admin-blocking operations work against the snapshot, so
/// the actual `LoggerState` lock isn't held across `push_blocking` or
/// other potentially-parking primitives.
pub(crate) struct StateSnapshot {
    pub(crate) slots: Vec<Arc<TransportSlot>>,
    pub(crate) global_format: Option<Arc<dyn Format<Input = LogInfo> + Send + Sync>>,
    pub(crate) global_level: Option<String>,
    pub(crate) levels: Option<LoggerLevels>,
    pub(crate) buffer: Arc<Mutex<VecDeque<Arc<LogInfo>>>>,
    pub(crate) event_senders: EventSenders,
}

impl StateSnapshot {
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

    fn format_for(&self, slot: &TransportSlot, entry: &LogInfo) -> Option<LogInfo> {
        match (&slot.transport_format, &self.global_format) {
            (Some(tf), _) => tf.transform(entry.clone()),
            (None, Some(gf)) => gf.transform(entry.clone()),
            (None, None) => Some(entry.clone()),
        }
    }

    /// Synchronously dispatch one entry across every slot. Two phases:
    ///
    /// 1. Non-blocking `try_push` to every slot whose level admits the
    ///    entry. Slots with room get it immediately. Drop policies
    ///    (`DropNewest` / `DropOldest`) resolve in this phase too. Only
    ///    `Block`-policy slots whose mailbox is full are deferred.
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
        let mut deferred_blocks: Vec<(usize, SlotMessage)> =
            Vec::with_capacity(self.slots.len());

        // Phase 1: try-push to every eligible slot.
        for (idx, slot) in self.slots.iter().enumerate() {
            if !self.passes_level(&entry.level, slot.level.as_ref()) {
                continue;
            }
            let Some(info) = self.format_for(slot, entry) else { continue };
            if let Some(deferred) = self.try_push_to_slot(slot, info) {
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

    /// Non-blocking push to a single slot. Returns `Some(msg)` only when
    /// the slot is `Block`-policy and the mailbox is full — caller must
    /// then call `push_blocking` in phase 2. Returns `None` on any other
    /// outcome (success, drop policy, slot torn down) — all of which the
    /// caller treats as "done" for this slot.
    fn try_push_to_slot(&self, slot: &TransportSlot, info: LogInfo) -> Option<SlotMessage> {
        match slot.mailbox_tx.try_push(SlotMessage::Entry(info)) {
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
