use logform::{FormatPipeline, FormattedEntry, LogInfo, Meta};
use std::{sync::Arc, time::Instant};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::{filter::LevelFilter, layer::Context, registry::LookupSpan, Layer};
use whatwg_streams::{CountQueuingStrategy, WritableStream};
use winston::{Logger, SpawnFn};
use winston_transport::{Transport, TransportSink};

/// Type-erased per-transport enqueue closure used by [`DirectLayer`]. Captures
/// a typed `WritableStreamDefaultWriter` so each `Transport` impl can have its
/// own concrete sink type while still living together in a `Vec`.
type EnqueueFn = Box<dyn Fn(FormattedEntry) + Send + Sync>;

struct SpanFields(Meta);

fn map_level(level: &Level) -> &'static str {
    match *level {
        Level::ERROR => "error",
        Level::WARN => "warn",
        Level::INFO => "info",
        Level::DEBUG => "debug",
        Level::TRACE => "trace",
    }
}

fn insert_location(meta: &mut Meta, m: &tracing::Metadata<'_>) {
    meta.insert(
        "target",
        serde_json::Value::String(m.target().to_string()),
    );
    if let Some(file) = m.file() {
        meta.insert("file", serde_json::Value::String(file.to_string()));
    }
    if let Some(line) = m.line() {
        meta.insert("line", serde_json::Value::Number(line.into()));
    }
}

/// Build a [`LogInfo`] from a tracing event, merging ancestor span fields.
fn build_log_info<S>(event: &Event<'_>, ctx: &Context<'_, S>) -> LogInfo
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    let level = map_level(event.metadata().level()).to_string();
    let mut fields = Meta::new();

    // Walk ancestor spans outermost → innermost so that more specific
    // (closer) spans override broader context. Cloning the span's `Meta`
    // preserves each key's `Cow` — static field names stay borrowed.
    if let Some(scope) = ctx.event_scope(event) {
        let spans: Vec<_> = scope.collect();
        for span in spans.iter().rev() {
            if let Some(sf) = span.extensions().get::<SpanFields>() {
                for (k, v) in sf.0.clone() {
                    fields.insert(k, v);
                }
            }
        }
    }

    event.record(&mut FieldVisitor(&mut fields));

    // "message" is tracing's conventional field name for the primary log line.
    let message = fields
        .remove("message")
        .map(|v| match v {
            serde_json::Value::String(s) => s,
            other => other.to_string(),
        })
        .unwrap_or_default();

    insert_location(&mut fields, event.metadata());

    LogInfo::from_parts(level, message, fields)
}

struct FieldVisitor<'a>(&'a mut Meta);

impl tracing::field::Visit for FieldVisitor<'_> {
    fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
        let number = serde_json::Number::from_f64(value).unwrap_or_else(|| 0.into());
        self.0
            .insert(field.name(), serde_json::Value::Number(number));
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.0.insert(
            field.name(),
            serde_json::Value::Number(value.into()),
        );
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.0.insert(
            field.name(),
            serde_json::Value::Number(value.into()),
        );
    }

    fn record_i128(&mut self, field: &tracing::field::Field, value: i128) {
        // serde_json::Number doesn't support i128; store as string to avoid silent truncation.
        self.0.insert(
            field.name(),
            serde_json::Value::String(value.to_string()),
        );
    }

    fn record_u128(&mut self, field: &tracing::field::Field, value: u128) {
        self.0.insert(
            field.name(),
            serde_json::Value::String(value.to_string()),
        );
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.0
            .insert(field.name(), serde_json::Value::Bool(value));
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.insert(
            field.name(),
            serde_json::Value::String(value.to_string()),
        );
    }

    fn record_error(
        &mut self,
        field: &tracing::field::Field,
        value: &(dyn std::error::Error + 'static),
    ) {
        // Walk the source chain; a single-entry chain is stored as a plain string
        // so existing consumers that expect a string field don't break.
        let mut chain: Vec<serde_json::Value> = vec![serde_json::Value::String(value.to_string())];
        let mut source = value.source();
        while let Some(err) = source {
            chain.push(serde_json::Value::String(err.to_string()));
            source = err.source();
        }
        let val = if chain.len() == 1 {
            chain.remove(0)
        } else {
            serde_json::Value::Array(chain)
        };
        self.0.insert(field.name(), val);
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        // tracing::field::Empty implements Debug as "Empty" — skip it rather than
        // polluting every log entry with a spurious "Empty" string.
        let s = format!("{value:?}");
        if s == "Empty" {
            return;
        }
        self.0
            .insert(field.name(), serde_json::Value::String(s));
    }
}

/// A lightweight [`tracing_subscriber::Layer`] that routes tracing events
/// directly into one or more transports — no [`Logger`] required.
///
/// Each transport runs its own per-transport `WritableStream` task, spawned
/// via the `SpawnFn` you pass to the builder. Events fan out via the writers'
/// fire-and-forget `enqueue`, so the tracing callback returns immediately.
/// A slow transport applies backpressure on its own writer's queue without
/// blocking other transports.
///
/// # Example
///
/// ```rust,no_run
/// use tracing_subscriber::prelude::*;
/// use winston_tracing::DirectLayer;
/// use winston::{default_spawner, transports};
///
/// tracing_subscriber::registry()
///     .with(
///         DirectLayer::builder(default_spawner())
///             .format(logform::json())
///             .transport(transports::stdout())
///             .build(),
///     )
///     .init();
///
/// tracing::info!(user_id = 42, "user logged in");
/// ```
pub struct DirectLayer {
    format: Option<Arc<FormatPipeline>>,
    transports: Vec<EnqueueFn>,
    min_level: Option<LevelFilter>,
}

impl DirectLayer {
    pub fn builder(spawn_fn: SpawnFn) -> DirectLayerBuilder {
        DirectLayerBuilder {
            spawn_fn,
            format: None,
            transports: Vec::new(),
            min_level: None,
        }
    }
}

pub struct DirectLayerBuilder {
    spawn_fn: SpawnFn,
    format: Option<Arc<FormatPipeline>>,
    transports: Vec<EnqueueFn>,
    min_level: Option<LevelFilter>,
}

impl DirectLayerBuilder {
    /// Set the logform format pipeline. All events pass through this before
    /// reaching transports. A format that returns `None` silently drops the entry.
    pub fn format<F>(mut self, format: F) -> Self
    where
        F: logform::IntoFormatPipeline,
    {
        self.format = Some(Arc::new(format.into_format_pipeline()));
        self
    }

    /// Add a transport. Each call spawns a per-transport `WritableStream`
    /// task using the builder's `SpawnFn`. Multiple transports each receive
    /// every (post-format) entry.
    pub fn transport<T>(mut self, transport: T) -> Self
    where
        T: Transport,
    {
        let spawn_for_stream = Arc::clone(&self.spawn_fn);
        let stream = WritableStream::builder(TransportSink(transport))
            .strategy(CountQueuingStrategy::new(1024))
            .spawn(move |fut| spawn_for_stream(fut));
        let (locked, writer) = stream
            .get_writer()
            .expect("DirectLayer: failed to acquire writer for transport");

        // Move `locked` and `writer` into the closure together — `_locked`
        // keeps the writer's exclusivity for as long as the closure (and
        // therefore the DirectLayer) is alive.
        let enqueue: EnqueueFn = Box::new(move |entry: FormattedEntry| {
            // Held only to keep the lock alive.
            let _ = &locked;
            let _ = writer.enqueue(entry);
        });
        self.transports.push(enqueue);
        self
    }

    /// Set the minimum log level. Events below this level are filtered out
    /// before any format or transport runs.
    ///
    /// Accepts the same strings as Winston: `"error"`, `"warn"`, `"info"`,
    /// `"debug"`, `"trace"`.
    pub fn level(mut self, level: impl AsRef<str>) -> Self {
        self.min_level = parse_level_filter(level.as_ref());
        self
    }

    pub fn build(self) -> DirectLayer {
        DirectLayer {
            format: self.format,
            transports: self.transports,
            min_level: self.min_level,
        }
    }
}

fn parse_level_filter(s: &str) -> Option<LevelFilter> {
    match s {
        "error" => Some(LevelFilter::ERROR),
        "warn" => Some(LevelFilter::WARN),
        "info" => Some(LevelFilter::INFO),
        "debug" => Some(LevelFilter::DEBUG),
        "trace" => Some(LevelFilter::TRACE),
        _ => None,
    }
}

impl<S> Layer<S> for DirectLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        ctx: Context<'_, S>,
    ) {
        let span = ctx.span(id).expect("span not found, this is a bug");
        let mut fields = Meta::new();
        // Seed with the span name so child events know which span they fired in.
        fields.insert(
            "span",
            serde_json::Value::String(span.name().to_string()),
        );
        attrs.record(&mut FieldVisitor(&mut fields));
        span.extensions_mut().insert(SpanFields(fields));
    }

    fn on_record(
        &self,
        id: &tracing::span::Id,
        values: &tracing::span::Record<'_>,
        ctx: Context<'_, S>,
    ) {
        let span = ctx.span(id).expect("span not found, this is a bug");
        let mut extensions = span.extensions_mut();
        if let Some(sf) = extensions.get_mut::<SpanFields>() {
            values.record(&mut FieldVisitor(&mut sf.0));
        }
    }

    fn enabled(&self, metadata: &tracing::Metadata<'_>, _ctx: Context<'_, S>) -> bool {
        self.min_level
            .map(|min| *metadata.level() <= min)
            .unwrap_or(true)
    }

    fn max_level_hint(&self) -> Option<LevelFilter> {
        self.min_level
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        if self.transports.is_empty() {
            return;
        }

        let info = build_log_info(event, &ctx);

        let entry = match &self.format {
            Some(fmt) => match fmt.apply(info) {
                Some(e) => e,
                None => return,
            },
            None => FormattedEntry::new(info, None),
        };

        // Fire-and-forget enqueue per transport. Backpressure is per-stream;
        // a slow transport doesn't block the others.
        for enqueue in &self.transports {
            enqueue(entry.clone());
        }
    }
}
struct SpanCreatedAt(Instant);

/// Controls which span lifecycle transitions emit a [`LogInfo`] entry.
///
/// By default nothing is emitted — only tracing events pass through.
/// Chain [`on_new`](SpanEvents::on_new) and [`on_close`](SpanEvents::on_close)
/// to opt into the phases you care about.
///
/// `on_close` entries include a `duration_ms` field.
///
/// # Example
///
/// ```rust,no_run
/// use winston_tracing::{SpanEvents, WinstonLayer};
/// use winston::Logger;
/// use winston_tracing::prelude::*;
/// use tracing_subscriber::prelude::*;
///
/// tracing_subscriber::registry()
///     .with(
///         Logger::builder()
///             .transport(winston::transports::stdout())
///             .build()
///             .layer()
///             .with_span_events(SpanEvents::default().on_close()),
///     )
///     .init();
/// ```
#[derive(Default, Clone)]
pub struct SpanEvents {
    new: bool,
    close: bool,
}

impl SpanEvents {
    pub fn on_new(mut self) -> Self {
        self.new = true;
        self
    }

    pub fn on_close(mut self) -> Self {
        self.close = true;
        self
    }
}

/// A [`tracing_subscriber::Layer`] that routes tracing events into a Winston [`Logger`].
///
/// Span fields are collected and merged into every event that fires within the span,
/// with child span fields overriding parent fields, and event fields overriding both.
///
/// For new code, prefer [`DirectLayer`] — it composes logform and transports
/// without the Logger's background thread.
///
/// Span lifecycle events (open/close) are opt-in via [`with_span_events`](WinstonLayer::with_span_events).
pub struct WinstonLayer {
    logger: Arc<Logger>,
    span_events: SpanEvents,
}

impl WinstonLayer {
    pub fn new(logger: impl Into<Arc<Logger>>) -> Self {
        Self {
            logger: logger.into(),
            span_events: SpanEvents::default(),
        }
    }

    /// Emit log entries for span lifecycle events.
    ///
    /// `on_close` entries include a `duration_ms` field.
    pub fn with_span_events(mut self, config: SpanEvents) -> Self {
        self.span_events = config;
        self
    }
}

/// Extension trait that lets a [`Logger`] (or `Arc<Logger>`) produce a
/// [`tracing_subscriber`] layer directly.
///
/// # Example
///
/// ```rust,no_run
/// use tracing_subscriber::prelude::*;
/// use winston::Logger;
/// use winston_tracing::prelude::*;
///
/// tracing_subscriber::registry()
///     .with(
///         Logger::builder()
///             .transport(winston::transports::stdout())
///             .build()
///             .layer(),
///     )
///     .init();
///
/// tracing::info!(user_id = 42, "user logged in");
/// ```
///
/// When you need a handle to the logger after handing it to the subscriber
/// (e.g. to flush on shutdown), wrap in `Arc` first:
///
/// ```rust,no_run
/// use std::sync::Arc;
/// use tracing_subscriber::prelude::*;
/// use winston::Logger;
/// use winston_tracing::prelude::*;
///
/// let logger = Arc::new(
///     Logger::builder()
///         .transport(winston::transports::stdout())
///         .build(),
/// );
///
/// tracing_subscriber::registry()
///     .with(Arc::clone(&logger).layer())
///     .init();
///
/// tracing::info!("hello");
/// logger.flush().unwrap();
/// ```
pub trait LoggerTracingExt {
    fn layer(self) -> WinstonLayer;
}

impl LoggerTracingExt for Logger {
    fn layer(self) -> WinstonLayer {
        WinstonLayer::new(self)
    }
}

impl LoggerTracingExt for Arc<Logger> {
    fn layer(self) -> WinstonLayer {
        WinstonLayer::new(self)
    }
}

impl<S> Layer<S> for WinstonLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        ctx: Context<'_, S>,
    ) {
        let span = ctx.span(id).expect("span not found, this is a bug");
        let span_name = span.name().to_string();

        let mut fields = Meta::new();
        // Seed with the span name so child events know which span they fired in.
        fields.insert(
            "span",
            serde_json::Value::String(span_name.clone()),
        );
        attrs.record(&mut FieldVisitor(&mut fields));

        {
            let mut extensions = span.extensions_mut();
            extensions.insert(SpanCreatedAt(Instant::now()));
            extensions.insert(SpanFields(fields.clone()));
        }

        if self.span_events.new {
            let mut meta = fields;
            meta.remove("span");
            let metadata = attrs.metadata();
            insert_location(&mut meta, metadata);
            let level = map_level(metadata.level()).to_string();
            self.logger.log(LogInfo::from_parts(level, span_name, meta));
        }
    }

    fn on_record(
        &self,
        id: &tracing::span::Id,
        values: &tracing::span::Record<'_>,
        ctx: Context<'_, S>,
    ) {
        let span = ctx.span(id).expect("span not found, this is a bug");
        let mut extensions = span.extensions_mut();
        if let Some(sf) = extensions.get_mut::<SpanFields>() {
            values.record(&mut FieldVisitor(&mut sf.0));
        }
    }

    fn enabled(&self, metadata: &tracing::Metadata<'_>, _ctx: Context<'_, S>) -> bool {
        self.logger
            .is_level_enabled(map_level(metadata.level()))
    }

    fn max_level_hint(&self) -> Option<LevelFilter> {
        for (level, filter) in [
            (Level::TRACE, LevelFilter::TRACE),
            (Level::DEBUG, LevelFilter::DEBUG),
            (Level::INFO, LevelFilter::INFO),
            (Level::WARN, LevelFilter::WARN),
            (Level::ERROR, LevelFilter::ERROR),
        ] {
            if self.logger.is_level_enabled(map_level(&level)) {
                return Some(filter);
            }
        }
        Some(LevelFilter::OFF)
    }

    fn on_close(&self, id: tracing::span::Id, ctx: Context<'_, S>) {
        if !self.span_events.close {
            return;
        }

        let span = ctx.span(&id).expect("span not found, this is a bug");
        let extensions = span.extensions();

        let elapsed_ms = extensions
            .get::<SpanCreatedAt>()
            .map(|t| t.0.elapsed().as_millis() as u64);

        let mut meta = if let Some(sf) = extensions.get::<SpanFields>() {
            let mut f = sf.0.clone();
            f.remove("span");
            f
        } else {
            Meta::new()
        };

        if let Some(ms) = elapsed_ms {
            meta.insert("duration_ms", serde_json::json!(ms));
        }

        let metadata = span.metadata();
        insert_location(&mut meta, metadata);
        let level = map_level(metadata.level()).to_string();
        self.logger
            .log(LogInfo::from_parts(level, span.name().to_string(), meta));
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        self.logger.log(build_log_info(event, &ctx));
    }
}

pub mod prelude {
    pub use super::LoggerTracingExt;
}

#[cfg(test)]
mod tests {
    use super::*;
    use logform::{FinalizeExt, Format};
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::prelude::*;
    use winston_transport::TransportResult;
    use winston::default_spawner;

    /// Test transport: pushes each entry into a shared `Vec`. Cloning shares
    /// the underlying `Arc<Mutex<Vec<LogInfo>>>` so the test thread retains a
    /// handle to inspect what was written even after the consumer takes the
    /// transport.
    #[derive(Clone)]
    struct CaptureTransport(Arc<Mutex<Vec<LogInfo>>>);

    impl Transport for CaptureTransport {
        async fn log(&mut self, entry: FormattedEntry) -> TransportResult<()> {
            self.0.lock().unwrap().push(entry.info);
            Ok(())
        }
    }

    fn capture() -> (CaptureTransport, Arc<Mutex<Vec<LogInfo>>>) {
        let store = Arc::new(Mutex::new(Vec::new()));
        (CaptureTransport(store.clone()), store)
    }

    /// Per-test-event grace period: enqueue is fire-and-forget into the
    /// per-transport WritableStream task, so test assertions need a moment
    /// for the task to drain. 100ms is generous for the in-memory writes
    /// these tests do; the alternative would be to also flush, which means
    /// taking a `Logger` route — that defeats the point of `DirectLayer`'s
    /// no-Logger story.
    fn drain() {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    #[test]
    fn direct_event_fields_become_meta() {
        let (transport, captured) = capture();
        let _guard = tracing_subscriber::registry()
            .with(DirectLayer::builder(default_spawner()).transport(transport).build())
            .set_default();

        tracing::info!(user_id = 42u64, "login");
        drain();

        let logs = captured.lock().unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].level, "info");
        assert_eq!(logs[0].message, "login");
        assert_eq!(logs[0].meta["user_id"], serde_json::json!(42u64));
    }

    #[test]
    fn direct_span_fields_propagate() {
        let (transport, captured) = capture();
        let _guard = tracing_subscriber::registry()
            .with(DirectLayer::builder(default_spawner()).transport(transport).build())
            .set_default();

        let span = tracing::info_span!("request", request_id = "abc");
        let _enter = span.enter();
        tracing::warn!("slow");
        drain();

        let logs = captured.lock().unwrap();
        assert_eq!(logs[0].meta["request_id"], serde_json::json!("abc"));
        assert_eq!(logs[0].meta["span"], serde_json::json!("request"));
    }

    #[test]
    fn direct_format_can_drop_entry() {
        struct DropAll;
        impl Format for DropAll {
            type Input = LogInfo;
            fn transform(&self, _: LogInfo) -> Option<LogInfo> {
                None
            }
        }

        let (transport, captured) = capture();
        let _guard = tracing_subscriber::registry()
            .with(
                DirectLayer::builder(default_spawner())
                    .format(DropAll.into_pipeline())
                    .transport(transport)
                    .build(),
            )
            .set_default();

        tracing::info!("should be dropped");
        drain();

        assert!(captured.lock().unwrap().is_empty());
    }

    #[test]
    fn direct_format_can_mutate_entry() {
        struct AddField;
        impl Format for AddField {
            type Input = LogInfo;
            fn transform(&self, mut info: LogInfo) -> Option<LogInfo> {
                info.meta
                    .insert("injected".to_string(), serde_json::json!(true));
                Some(info)
            }
        }

        let (transport, captured) = capture();
        let _guard = tracing_subscriber::registry()
            .with(
                DirectLayer::builder(default_spawner())
                    .format(AddField.into_pipeline())
                    .transport(transport)
                    .build(),
            )
            .set_default();

        tracing::info!("hello");
        drain();

        let logs = captured.lock().unwrap();
        assert_eq!(logs[0].meta["injected"], serde_json::json!(true));
    }

    #[test]
    fn direct_multiple_transports_each_receive_entry() {
        let (t1, c1) = capture();
        let (t2, c2) = capture();
        let _guard = tracing_subscriber::registry()
            .with(DirectLayer::builder(default_spawner()).transport(t1).transport(t2).build())
            .set_default();

        tracing::info!("broadcast");
        drain();

        assert_eq!(c1.lock().unwrap().len(), 1);
        assert_eq!(c2.lock().unwrap().len(), 1);
    }

    #[test]
    fn direct_level_filter_drops_below_min() {
        let (transport, captured) = capture();
        let _guard = tracing_subscriber::registry()
            .with(
                DirectLayer::builder(default_spawner())
                    .level("warn")
                    .transport(transport)
                    .build(),
            )
            .set_default();

        tracing::info!("filtered out");
        tracing::debug!("also filtered");
        tracing::warn!("passes");
        tracing::error!("also passes");
        drain();

        let logs = captured.lock().unwrap();
        assert_eq!(logs.len(), 2);
        assert_eq!(logs[0].level, "warn");
        assert_eq!(logs[1].level, "error");
    }

    #[test]
    fn direct_no_transports_does_not_panic() {
        let _guard = tracing_subscriber::registry()
            .with(DirectLayer::builder(default_spawner()).build())
            .set_default();

        tracing::info!("no transports configured");
    }

    fn make_logger_and_capture() -> (Arc<Logger>, Arc<Mutex<Vec<LogInfo>>>) {
        let (transport, captured) = capture();
        let logger = Arc::new(
            Logger::builder()
                .level("trace")
                .format(logform::passthrough().into_pipeline())
                .transport(transport)
                .build(),
        );
        (logger, captured)
    }

    #[test]
    fn winston_event_fields_become_meta() {
        let (logger, captured) = make_logger_and_capture();
        let _guard = tracing_subscriber::registry()
            .with(Arc::clone(&logger).layer())
            .set_default();

        tracing::info!(user_id = 42u64, "login");
        logger.flush().unwrap();

        let logs = captured.lock().unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].level, "info");
        assert_eq!(logs[0].message, "login");
        assert_eq!(logs[0].meta["user_id"], serde_json::json!(42u64));
    }

    #[test]
    fn winston_span_fields_propagate_into_events() {
        let (logger, captured) = make_logger_and_capture();
        let _guard = tracing_subscriber::registry()
            .with(Arc::clone(&logger).layer())
            .set_default();

        let span = tracing::info_span!("request", request_id = "abc-123");
        let _enter = span.enter();
        tracing::warn!("something went wrong");
        logger.flush().unwrap();

        let logs = captured.lock().unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].meta["request_id"], serde_json::json!("abc-123"));
        assert_eq!(logs[0].meta["span"], serde_json::json!("request"));
    }

    #[test]
    fn winston_event_fields_override_span_fields() {
        let (logger, captured) = make_logger_and_capture();
        let _guard = tracing_subscriber::registry()
            .with(Arc::clone(&logger).layer())
            .set_default();

        let span = tracing::info_span!("work", key = "from-span");
        let _enter = span.enter();
        tracing::info!(key = "from-event", "override");
        logger.flush().unwrap();

        let logs = captured.lock().unwrap();
        assert_eq!(logs[0].meta["key"], serde_json::json!("from-event"));
    }

    #[test]
    fn winston_level_mapping() {
        let (logger, captured) = make_logger_and_capture();
        let _guard = tracing_subscriber::registry()
            .with(Arc::clone(&logger).layer())
            .set_default();

        tracing::error!("e");
        tracing::warn!("w");
        tracing::info!("i");
        tracing::debug!("d");
        tracing::trace!("t");
        logger.flush().unwrap();

        let logs = captured.lock().unwrap();
        let levels: Vec<&str> = logs.iter().map(|l| l.level.as_str()).collect();
        assert_eq!(levels, ["error", "warn", "info", "debug", "trace"]);
    }

    #[test]
    fn on_close_emits_entry_with_duration() {
        let (logger, captured) = make_logger_and_capture();
        let _guard = tracing_subscriber::registry()
            .with(
                Arc::clone(&logger)
                    .layer()
                    .with_span_events(SpanEvents::default().on_close()),
            )
            .set_default();

        {
            let span = tracing::info_span!("db_query", table = "users");
            let _enter = span.enter();
            // span drops here, triggering on_close
        }
        logger.flush().unwrap();

        let logs = captured.lock().unwrap();
        assert_eq!(logs.len(), 1);
        let entry = &logs[0];
        assert_eq!(entry.level, "info");
        assert_eq!(entry.message, "db_query");
        assert_eq!(entry.meta["table"], serde_json::json!("users"));
        assert!(
            entry.meta.contains_key("duration_ms"),
            "on_close entry must include duration_ms"
        );
    }

    #[test]
    fn on_new_emits_entry() {
        let (logger, captured) = make_logger_and_capture();
        let _guard = tracing_subscriber::registry()
            .with(
                Arc::clone(&logger)
                    .layer()
                    .with_span_events(SpanEvents::default().on_new()),
            )
            .set_default();

        let span = tracing::info_span!("handler", route = "/api/v1");
        let _enter = span.enter();
        logger.flush().unwrap();

        let logs = captured.lock().unwrap();
        assert_eq!(logs.len(), 1);
        let entry = &logs[0];
        assert_eq!(entry.level, "info");
        assert_eq!(entry.message, "handler");
        assert_eq!(entry.meta["route"], serde_json::json!("/api/v1"));
        assert!(!entry.meta.contains_key("duration_ms"));
    }

    #[test]
    fn on_new_and_close_both_emit() {
        let (logger, captured) = make_logger_and_capture();
        let _guard = tracing_subscriber::registry()
            .with(
                Arc::clone(&logger)
                    .layer()
                    .with_span_events(SpanEvents::default().on_new().on_close()),
            )
            .set_default();

        {
            let span = tracing::info_span!("tx");
            let _enter = span.enter();
        }
        logger.flush().unwrap();

        let logs = captured.lock().unwrap();
        assert_eq!(logs.len(), 2);
        // open entry has no duration, close entry does
        assert!(!logs[0].meta.contains_key("duration_ms"));
        assert!(logs[1].meta.contains_key("duration_ms"));
    }

    #[test]
    fn no_span_events_by_default() {
        let (logger, captured) = make_logger_and_capture();
        let _guard = tracing_subscriber::registry()
            .with(Arc::clone(&logger).layer())
            .set_default();

        {
            let span = tracing::info_span!("silent");
            let _enter = span.enter();
        }
        logger.flush().unwrap();

        assert!(
            captured.lock().unwrap().is_empty(),
            "span lifecycle must not emit by default"
        );
    }

    #[test]
    fn error_source_chain_is_captured() {
        #[derive(Debug)]
        struct Inner;
        impl std::fmt::Display for Inner {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "inner cause")
            }
        }
        impl std::error::Error for Inner {}

        #[derive(Debug)]
        struct Outer(Inner);
        impl std::fmt::Display for Outer {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "outer error")
            }
        }
        impl std::error::Error for Outer {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }

        let (logger, captured) = make_logger_and_capture();
        let _guard = tracing_subscriber::registry()
            .with(Arc::clone(&logger).layer())
            .set_default();

        let err = Outer(Inner);
        tracing::error!(err = &err as &dyn std::error::Error, "request failed");
        logger.flush().unwrap();

        let logs = captured.lock().unwrap();
        let chain = logs[0].meta["err"].as_array().expect("should be array");
        assert_eq!(chain[0], serde_json::json!("outer error"));
        assert_eq!(chain[1], serde_json::json!("inner cause"));
    }
}
