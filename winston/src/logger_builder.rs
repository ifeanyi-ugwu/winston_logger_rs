use crate::{
    logger_options::LoggerOptions,
    logger_transport::IntoLoggerTransport,
    pipeline::{self, SpawnFn},
    Logger,
};
use logform::{Format, LogInfo};
use std::collections::HashMap;

pub struct LoggerBuilder {
    options: LoggerOptions,
    spawn_fn: Option<SpawnFn>,
}

impl Default for LoggerBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl LoggerBuilder {
    pub fn new() -> Self {
        LoggerBuilder {
            options: LoggerOptions::default(),
            spawn_fn: None,
        }
    }

    pub fn spawner(mut self, spawn_fn: SpawnFn) -> Self {
        self.spawn_fn = Some(spawn_fn);
        self
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

    pub fn build(self) -> Logger {
        let spawn_fn = self
            .spawn_fn
            .unwrap_or_else(pipeline::default_spawner);
        Logger::new_with_spawner(Some(self.options), spawn_fn)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        let logger = LoggerBuilder::new().level("warn").build();

        let state = logger.shared_state.read();
        assert_eq!(state.options.level.as_deref(), Some("warn"));
    }
}
