use logform::LogInfo;
use std::{collections::HashMap, sync::Arc};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::{layer::Context, registry::LookupSpan, Layer};
use winston::Logger;

struct SpanFields(HashMap<String, serde_json::Value>);

/// A [`tracing_subscriber::Layer`] that routes tracing events into a Winston [`Logger`].
///
/// Span fields are collected and merged into every event that fires within the span,
/// with child span fields overriding parent fields, and event fields overriding both.
pub struct WinstonLayer {
    logger: Arc<Logger>,
}

impl WinstonLayer {
    pub fn new(logger: impl Into<Arc<Logger>>) -> Self {
        Self {
            logger: logger.into(),
        }
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
        let mut fields = HashMap::new();
        // Seed with the span name so child events know which span they fired in.
        fields.insert(
            "span".to_string(),
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
        self.logger.is_level_enabled_fast(map_level(metadata.level()))
    }

    fn max_level_hint(&self) -> Option<tracing_subscriber::filter::LevelFilter> {
        use tracing::Level;
        use tracing_subscriber::filter::LevelFilter;
        for (level, filter) in [
            (Level::TRACE, LevelFilter::TRACE),
            (Level::DEBUG, LevelFilter::DEBUG),
            (Level::INFO, LevelFilter::INFO),
            (Level::WARN, LevelFilter::WARN),
            (Level::ERROR, LevelFilter::ERROR),
        ] {
            if self.logger.is_level_enabled_fast(map_level(&level)) {
                return Some(filter);
            }
        }
        Some(LevelFilter::OFF)
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let level = map_level(event.metadata().level()).to_string();

        let mut fields: HashMap<String, serde_json::Value> = HashMap::new();

        // Walk ancestor spans outermost → innermost so that more specific
        // (closer) spans override broader context.
        if let Some(scope) = ctx.event_scope(event) {
            let spans: Vec<_> = scope.collect();
            for span in spans.iter().rev() {
                if let Some(sf) = span.extensions().get::<SpanFields>() {
                    for (k, v) in &sf.0 {
                        fields.insert(k.clone(), v.clone());
                    }
                }
            }
        }

        // Event fields are most specific and override span context.
        event.record(&mut FieldVisitor(&mut fields));

        // "message" is tracing's conventional field name for the primary log line.
        let message = fields
            .remove("message")
            .map(|v| match v {
                serde_json::Value::String(s) => s,
                other => other.to_string(),
            })
            .unwrap_or_default();

        fields.insert(
            "target".to_string(),
            serde_json::Value::String(event.metadata().target().to_string()),
        );

        if let Some(file) = event.metadata().file() {
            fields.insert(
                "file".to_string(),
                serde_json::Value::String(file.to_string()),
            );
        }
        if let Some(line) = event.metadata().line() {
            fields.insert("line".to_string(), serde_json::Value::Number(line.into()));
        }

        self.logger.log(LogInfo {
            level,
            message,
            meta: fields,
        });
    }
}

fn map_level(level: &Level) -> &'static str {
    match *level {
        Level::ERROR => "error",
        Level::WARN => "warn",
        Level::INFO => "info",
        Level::DEBUG => "debug",
        Level::TRACE => "trace",
    }
}

struct FieldVisitor<'a>(&'a mut HashMap<String, serde_json::Value>);

impl tracing::field::Visit for FieldVisitor<'_> {
    fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
        let number = serde_json::Number::from_f64(value).unwrap_or_else(|| 0.into());
        self.0
            .insert(field.name().to_string(), serde_json::Value::Number(number));
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.0.insert(
            field.name().to_string(),
            serde_json::Value::Number(value.into()),
        );
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.0.insert(
            field.name().to_string(),
            serde_json::Value::Number(value.into()),
        );
    }

    fn record_i128(&mut self, field: &tracing::field::Field, value: i128) {
        // serde_json::Number doesn't support i128; store as string to avoid silent truncation.
        self.0.insert(
            field.name().to_string(),
            serde_json::Value::String(value.to_string()),
        );
    }

    fn record_u128(&mut self, field: &tracing::field::Field, value: u128) {
        self.0.insert(
            field.name().to_string(),
            serde_json::Value::String(value.to_string()),
        );
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.0
            .insert(field.name().to_string(), serde_json::Value::Bool(value));
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.insert(
            field.name().to_string(),
            serde_json::Value::String(value.to_string()),
        );
    }

    fn record_error(
        &mut self,
        field: &tracing::field::Field,
        value: &(dyn std::error::Error + 'static),
    ) {
        self.0.insert(
            field.name().to_string(),
            serde_json::Value::String(value.to_string()),
        );
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.insert(
            field.name().to_string(),
            serde_json::Value::String(format!("{value:?}")),
        );
    }
}

pub mod prelude {
    pub use super::LoggerTracingExt;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::prelude::*;
    use winston_transport::Transport;

    #[derive(Clone)]
    struct CaptureTransport(Arc<Mutex<Vec<LogInfo>>>);

    impl Transport<LogInfo> for CaptureTransport {
        fn log(&self, info: LogInfo) {
            self.0.lock().unwrap().push(info);
        }
    }

    // level("trace") captures everything; passthrough() leaves LogInfo fields untouched
    // so assertions see raw level/message/meta rather than the default formatted message string.
    fn make_logger_and_capture() -> (Arc<Logger>, Arc<Mutex<Vec<LogInfo>>>) {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let logger = Arc::new(
            Logger::builder()
                .level("trace")
                .format(logform::passthrough())
                .transport(CaptureTransport(captured.clone()))
                .build(),
        );
        (logger, captured)
    }

    #[test]
    fn event_fields_become_meta() {
        let (logger, captured) = make_logger_and_capture();
        let _guard = tracing_subscriber::registry()
            .with(Arc::clone(&logger).layer())
            .set_default();

        tracing::info!(user_id = 42u64, "login");
        logger.flush().unwrap();

        let logs = captured.lock().unwrap();
        assert_eq!(logs.len(), 1);
        let entry = &logs[0];
        assert_eq!(entry.level, "info");
        assert_eq!(entry.message, "login");
        assert_eq!(entry.meta["user_id"], serde_json::json!(42u64));
    }

    #[test]
    fn span_fields_propagate_into_events() {
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
        let entry = &logs[0];
        assert_eq!(entry.level, "warn");
        assert_eq!(entry.message, "something went wrong");
        assert_eq!(entry.meta["request_id"], serde_json::json!("abc-123"));
        assert_eq!(entry.meta["span"], serde_json::json!("request"));
    }

    #[test]
    fn event_fields_override_span_fields() {
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
    fn level_mapping() {
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
}
