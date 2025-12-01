use crate::{
    logger_options::{BackpressureStrategy, LoggerOptions},
    logger_transport::IntoLoggerTransport,
    Logger,
};
use logform::{Format, LogInfo};
use std::collections::HashMap;

pub struct LoggerBuilder {
    options: LoggerOptions,
}

impl LoggerBuilder {
    pub fn new() -> Self {
        LoggerBuilder {
            options: LoggerOptions::default(),
        }
    }

    pub fn level<T: Into<String>>(mut self, level: T) -> Self {
        self.options = self.options.level(level);
        self
    }

    pub fn format<F>(mut self, format: F) -> Self
    where
        F: Format<Input = LogInfo> + Send + Sync + 'static,
    {
        self.options = self.options.format(format);
        self
    }

    pub fn transport(mut self, transport: impl IntoLoggerTransport) -> Self {
        self.options = self.options.transport(transport);
        self
    }

    pub fn transports<I>(mut self, transports: I) -> Self
    where
        I: IntoIterator,
        I::Item: IntoLoggerTransport,
    {
        self.options = self.options.transports(transports);
        self
    }

    pub fn levels(mut self, levels: HashMap<String, u8>) -> Self {
        self.options = self.options.levels(levels);
        self
    }

    pub fn channel_capacity(mut self, capacity: usize) -> Self {
        self.options = self.options.channel_capacity(capacity);
        self
    }

    pub fn backpressure_strategy(mut self, strategy: BackpressureStrategy) -> Self {
        self.options = self.options.backpressure_strategy(strategy);
        self
    }

    pub fn build(self) -> Logger {
        Logger::new(Some(self.options))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BackpressureStrategy;
    use std::collections::HashMap;

    #[test]
    fn test_builder_default_construction() {
        let builder = LoggerBuilder::new();
        let logger = builder.build();

        let state = logger.shared_state.read();
        assert!(state.options.levels.is_some());
        assert_eq!(state.options.level.as_deref(), Some("info"));
    }

    #[test]
    fn test_builder_with_level() {
        let logger = LoggerBuilder::new().level("debug").build();

        let state = logger.shared_state.read();
        assert_eq!(state.options.level.as_deref(), Some("debug"));
    }

    #[test]
    fn test_builder_with_channel_capacity() {
        let logger = LoggerBuilder::new().channel_capacity(2048).build();

        let state = logger.shared_state.read();
        assert_eq!(state.options.channel_capacity, Some(2048));
    }

    #[test]
    fn test_builder_with_backpressure_strategy() {
        let logger = LoggerBuilder::new()
            .backpressure_strategy(BackpressureStrategy::DropOldest)
            .build();

        let state = logger.shared_state.read();
        assert!(matches!(
            state.options.backpressure_strategy,
            Some(BackpressureStrategy::DropOldest)
        ));
    }

    #[test]
    fn test_builder_with_custom_levels() {
        let mut custom_levels = HashMap::new();
        custom_levels.insert("critical".to_string(), 0);
        custom_levels.insert("normal".to_string(), 5);

        let logger = LoggerBuilder::new().levels(custom_levels.clone()).build();

        let state = logger.shared_state.read();
        let levels = state.options.levels.as_ref().unwrap();
        assert_eq!(levels.get_severity("critical"), Some(0));
        assert_eq!(levels.get_severity("normal"), Some(5));
    }

    #[test]
    fn test_builder_chaining() {
        let logger = LoggerBuilder::new()
            .level("warn")
            .channel_capacity(512)
            .backpressure_strategy(BackpressureStrategy::Block)
            .build();

        let state = logger.shared_state.read();
        assert_eq!(state.options.level.as_deref(), Some("warn"));
        assert_eq!(state.options.channel_capacity, Some(512));
        assert!(matches!(
            state.options.backpressure_strategy,
            Some(BackpressureStrategy::Block)
        ));
    }
}
