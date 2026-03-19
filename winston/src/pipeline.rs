use std::{
    collections::VecDeque,
    sync::{Arc, Condvar, Mutex},
};

use futures::channel::mpsc as fmpsc;
use futures::StreamExt;
use logform::{Format, LogInfo};
use whatwg_streams::{
    CountQueuingStrategy, ReadableSource, ReadableStream, ReadableStreamDefaultController,
    StreamResult, WritableSink, WritableStream, WritableStreamDefaultController,
};

use crate::{
    logger::TransportHandle, logger_levels::LoggerLevels, logger_options::LoggerOptions,
    logger_transport::LoggerTransport,
};

// ── Messages flowing through the main pipeline ──────────────────────────────

pub enum PipelineMessage {
    Entry(Arc<LogInfo>),
    Flush(Arc<(Mutex<bool>, Condvar)>),
    /// Add a transport at runtime; the FanoutSink spawns its task.
    AddTransport {
        handle: TransportHandle,
        transport: LoggerTransport<LogInfo>,
    },
    /// Remove a transport by handle; the FanoutSink aborts its task.
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
        transports: Vec<(TransportHandle, LoggerTransport<LogInfo>)>,
    },
    Shutdown,
}

// SAFETY: every variant's payload is Send + Sync.
unsafe impl Send for PipelineMessage {}
unsafe impl Sync for PipelineMessage {}

// ── Per-transport messages ───────────────────────────────────────────────────

enum TransportMessage {
    Entry(Arc<LogInfo>),
    Flush(futures::channel::oneshot::Sender<()>),
}

// ── Pipeline source: wraps an unbounded mpsc Receiver ───────────────────────

pub struct PipelineSource {
    rx: fmpsc::UnboundedReceiver<PipelineMessage>,
}

impl PipelineSource {
    pub fn new(rx: fmpsc::UnboundedReceiver<PipelineMessage>) -> Self {
        Self { rx }
    }
}

impl ReadableSource<PipelineMessage> for PipelineSource {
    async fn pull(
        &mut self,
        ctrl: &mut ReadableStreamDefaultController<PipelineMessage>,
    ) -> StreamResult<()> {
        match self.rx.next().await {
            Some(msg) => ctrl.enqueue(msg)?,
            None => ctrl.close()?,
        }
        Ok(())
    }
}

// ── Per-transport task slot ──────────────────────────────────────────────────

struct TransportSlot {
    handle: TransportHandle,
    level: Option<String>,
    tx: fmpsc::UnboundedSender<TransportMessage>,
    task_handle: tokio::task::JoinHandle<()>,
}

// ── Fanout sink ──────────────────────────────────────────────────────────────

/// Receives pipeline messages, fans log entries out to per-transport tasks,
/// and handles dynamic transport add/remove without any external locking.
///
/// Runs entirely inside the streams pipeline (a single tokio task spawned by
/// the writable stream), so `&mut self` access is always exclusive.
pub struct FanoutSink {
    transport_tasks: Vec<TransportSlot>,
    global_format: Option<Arc<dyn Format<Input = LogInfo> + Send + Sync>>,
    global_level: Option<String>,
    levels: Option<LoggerLevels>,
    /// Entries buffered before the first transport arrives.
    buffer: Arc<Mutex<VecDeque<Arc<LogInfo>>>>,
}

impl FanoutSink {
    pub fn new(
        global_format: Option<Arc<dyn Format<Input = LogInfo> + Send + Sync>>,
        global_level: Option<String>,
        levels: Option<LoggerLevels>,
        buffer: Arc<Mutex<VecDeque<Arc<LogInfo>>>>,
        initial_transports: Vec<(TransportHandle, LoggerTransport<LogInfo>)>,
    ) -> Self {
        let mut sink = Self {
            transport_tasks: Vec::new(),
            global_format,
            global_level,
            levels,
            buffer,
        };
        for (handle, transport) in initial_transports {
            sink.spawn_transport(handle, transport);
        }
        sink
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

    fn spawn_transport(&mut self, handle: TransportHandle, transport: LoggerTransport<LogInfo>) {
        let (tx, rx) = fmpsc::unbounded::<TransportMessage>();
        let global_fmt = self.global_format.clone();
        let level = transport.get_level().cloned();
        let task_handle = tokio::spawn(run_transport_task(rx, transport, global_fmt));
        self.transport_tasks.push(TransportSlot {
            handle,
            level,
            tx,
            task_handle,
        });
    }

    fn abort_transport(&mut self, handle: TransportHandle) {
        if let Some(pos) = self.transport_tasks.iter().position(|s| s.handle == handle) {
            let slot = self.transport_tasks.remove(pos);
            slot.task_handle.abort();
        }
    }

    fn drain_buffer_to_slots(&mut self) {
        let buffered: Vec<Arc<LogInfo>> = {
            let mut buf = self.buffer.lock().unwrap();
            buf.drain(..).collect()
        };
        for entry in buffered {
            self.fan_entry_to_slots(&entry);
        }
    }

    fn fan_entry_to_slots(&self, entry: &Arc<LogInfo>) {
        for slot in &self.transport_tasks {
            if self.passes_level(&entry.level, slot.level.as_ref()) {
                let _ = slot
                    .tx
                    .unbounded_send(TransportMessage::Entry(Arc::clone(entry)));
            }
        }
    }

    fn process_entry(&mut self, entry: Arc<LogInfo>) {
        if entry.message.is_empty() && entry.meta.is_empty() {
            return;
        }

        if self.transport_tasks.is_empty() {
            self.buffer.lock().unwrap().push_back(Arc::clone(&entry));
            eprintln!(
                "[winston] Attempt to write logs with no transports, which can increase memory usage: {}",
                entry.message
            );
            return;
        }

        self.drain_buffer_to_slots();
        self.fan_entry_to_slots(&entry);
    }

    async fn process_flush(&self, flush_complete: Arc<(Mutex<bool>, Condvar)>) {
        let rxs: Vec<_> = self
            .transport_tasks
            .iter()
            .map(|slot| {
                let (tx, rx) = futures::channel::oneshot::channel::<()>();
                let _ = slot.tx.unbounded_send(TransportMessage::Flush(tx));
                rx
            })
            .collect();

        let _ = futures::future::join_all(rxs).await;

        let (lock, cvar) = &*flush_complete;
        let mut done = lock.lock().unwrap();
        *done = true;
        cvar.notify_one();
    }

    fn clear_all_transports(&mut self) {
        for slot in self.transport_tasks.drain(..) {
            slot.task_handle.abort();
        }
    }
}

impl WritableSink<PipelineMessage> for FanoutSink {
    async fn write(
        &mut self,
        msg: PipelineMessage,
        _ctrl: &mut WritableStreamDefaultController,
    ) -> StreamResult<()> {
        match msg {
            PipelineMessage::Entry(entry) => self.process_entry(entry),

            PipelineMessage::Flush(fc) => self.process_flush(fc).await,

            PipelineMessage::AddTransport { handle, transport } => {
                self.spawn_transport(handle, transport);
                // Drain any buffer now that we have at least one transport.
                self.drain_buffer_to_slots();
            }

            PipelineMessage::RemoveTransport(handle) => self.abort_transport(handle),

            PipelineMessage::Reconfigure {
                format,
                level,
                levels,
            } => {
                self.global_format = format;
                self.global_level = level;
                self.levels = levels;
            }

            PipelineMessage::Configure {
                format,
                level,
                levels,
                transports,
            } => {
                self.clear_all_transports();
                self.global_format = format;
                self.global_level = level;
                self.levels = levels;
                for (handle, transport) in transports {
                    self.spawn_transport(handle, transport);
                }
                self.drain_buffer_to_slots();
            }

            PipelineMessage::Shutdown => {
                self.clear_all_transports();
            }
        }
        Ok(())
    }

    async fn close(mut self) -> StreamResult<()> {
        self.clear_all_transports();
        Ok(())
    }
}

// ── Per-transport async task ─────────────────────────────────────────────────

async fn run_transport_task(
    mut rx: fmpsc::UnboundedReceiver<TransportMessage>,
    transport: LoggerTransport<LogInfo>,
    global_format: Option<Arc<dyn Format<Input = LogInfo> + Send + Sync>>,
) {
    while let Some(msg) = rx.next().await {
        match msg {
            TransportMessage::Entry(entry) => {
                let formatted = match (transport.get_format(), &global_format) {
                    (Some(tf), _) => tf.transform((*entry).clone()),
                    (None, Some(lf)) => lf.transform((*entry).clone()),
                    (None, None) => Some((*entry).clone()),
                };
                if let Some(info) = formatted {
                    transport.get_transport().log(info);
                }
            }
            TransportMessage::Flush(tx) => {
                let _ = transport.get_transport().flush();
                let _ = tx.send(());
            }
        }
    }
}

// ── Pipeline constructor ─────────────────────────────────────────────────────

/// Builds and returns the spawned pipeline channel sender.
///
/// Spawns two tasks on the current tokio runtime:
///   1. The ReadableStream task (drives `PipelineSource`)
///   2. The WritableStream task (drives `FanoutSink`)
///
/// The returned sender is the entry point: push `PipelineMessage`s into it.
pub fn build_pipeline(
    options: &LoggerOptions,
    buffer: Arc<Mutex<VecDeque<Arc<LogInfo>>>>,
) -> fmpsc::UnboundedSender<PipelineMessage> {
    let (tx, rx) = fmpsc::unbounded::<PipelineMessage>();

    let global_format = options.format.clone();
    let global_level = options.level.clone();
    let levels = options.levels.clone();
    let initial_transports = options.transports.clone().unwrap_or_default();

    let sink = FanoutSink::new(
        global_format,
        global_level,
        levels,
        buffer,
        initial_transports,
    );
    let source = PipelineSource::new(rx);

    let readable = ReadableStream::builder(source)
        .strategy(CountQueuingStrategy::new(1))
        .spawn(|fut| {
            tokio::spawn(fut);
        });

    let writable = WritableStream::builder(sink)
        .strategy(CountQueuingStrategy::new(1))
        .spawn(|fut| {
            tokio::spawn(fut);
        });

    // Spawn the pipe loop as its own task so it drives itself to completion.
    tokio::spawn(async move {
        let _ = readable.pipe_to(&writable, None).await;
    });

    tx
}
